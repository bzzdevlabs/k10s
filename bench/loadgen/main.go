// loadgen: minimal HTTP/1.1 load generator (stdlib only, raw net.Conn).
// Closed loop (-conns N) or open loop (-rate R, latency measured from the
// intended send time, so queueing delay is included: no coordinated omission).
// Open loop also reports latency_us_jc: the same, minus the loadgen's own
// time.Sleep wake-up lateness when the sender was idle (see the worker loop),
// and wakeup_late_us (that lateness), since Go timers can wake up to ~1 ms late.
package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"math/bits"
	"net"
	"os"
	"sort"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
)

// ---- log-linear histogram (µs), ~1% relative error ----
const (
	subBits = 6
	subCnt  = 1 << (subBits + 1) // 128 linear buckets then 64 per power of two
	nBkt    = subCnt + (64-subBits-1)*(1<<subBits)
)

type hist struct {
	c   [nBkt]uint64
	n   uint64
	max uint64
}

func idx(v uint64) int {
	if v < subCnt {
		return int(v)
	}
	e := bits.Len64(v) - (subBits + 1)
	return subCnt + (e-1)*(1<<subBits) + int(v>>uint(e)) - (1 << subBits)
}
func low(i int) uint64 {
	if i < subCnt {
		return uint64(i)
	}
	k := i - subCnt
	e := k/(1<<subBits) + 1
	m := k%(1<<subBits) + (1 << subBits)
	return uint64(m) << uint(e)
}
func (h *hist) add(v uint64) {
	h.c[idx(v)]++
	h.n++
	if v > h.max {
		h.max = v
	}
}
func (h *hist) merge(o *hist) {
	for i := range h.c {
		h.c[i] += o.c[i]
	}
	h.n += o.n
	if o.max > h.max {
		h.max = o.max
	}
}
func (h *hist) q(p float64) uint64 {
	if h.n == 0 {
		return 0
	}
	target := uint64(float64(h.n)*p/100 + 0.999999)
	if target < 1 {
		target = 1
	}
	var acc uint64
	for i, c := range h.c {
		acc += c
		if acc >= target {
			lo := low(i)
			hi := low(i + 1)
			v := (lo + hi) / 2
			if i < subCnt {
				v = lo
			}
			if v > h.max {
				v = h.max
			}
			return v
		}
	}
	return h.max
}

// ---- minimal HTTP/1.1 client ----
type client struct {
	addr    string
	req     []byte
	timeout time.Duration
	conn    net.Conn
	br      *bufio.Reader
}

type errKind string

const (
	eConnect errKind = "connect"
	eWrite   errKind = "write"
	eRead    errKind = "read"
	eTimeout errKind = "timeout"
	eStatus  errKind = "status"
	eProto   errKind = "protocol"
)

func (c *client) close() {
	if c.conn != nil {
		c.conn.Close()
		c.conn = nil
	}
}

func classify(err error, def errKind) errKind {
	var ne net.Error
	if errors.As(err, &ne) && ne.Timeout() {
		return eTimeout
	}
	return def
}

// do performs one request. Returns "" on success.
func (c *client) do() errKind {
	if c.conn == nil {
		conn, err := net.DialTimeout("tcp", c.addr, c.timeout)
		if err != nil {
			return classify(err, eConnect)
		}
		if tc, ok := conn.(*net.TCPConn); ok {
			tc.SetNoDelay(true)
		}
		c.conn = conn
		c.br = bufio.NewReaderSize(conn, 16<<10)
	}
	c.conn.SetDeadline(time.Now().Add(c.timeout))
	if _, err := c.conn.Write(c.req); err != nil {
		c.close()
		return classify(err, eWrite)
	}
	line, err := c.br.ReadSlice('\n')
	if err != nil {
		c.close()
		return classify(err, eRead)
	}
	// "HTTP/1.1 200 OK"
	if len(line) < 12 || !bytes.HasPrefix(line, []byte("HTTP/1.")) {
		c.close()
		return eProto
	}
	status, _ := strconv.Atoi(string(line[9:12]))
	clen := int64(-1)
	chunked := false
	closeAfter := line[7] == '0'
	for {
		l, err := c.br.ReadSlice('\n')
		if err != nil {
			c.close()
			return classify(err, eRead)
		}
		if len(l) <= 2 {
			break
		}
		if l[0]|0x20 == 'c' || l[0]|0x20 == 't' {
			s := strings.ToLower(string(l))
			switch {
			case strings.HasPrefix(s, "content-length:"):
				clen, _ = strconv.ParseInt(strings.TrimSpace(s[15:]), 10, 64)
			case strings.HasPrefix(s, "transfer-encoding:") && strings.Contains(s, "chunked"):
				chunked = true
			case strings.HasPrefix(s, "connection:") && strings.Contains(s, "close"):
				closeAfter = true
			}
		}
	}
	switch {
	case chunked:
		for {
			l, err := c.br.ReadSlice('\n')
			if err != nil {
				c.close()
				return classify(err, eRead)
			}
			n, err := strconv.ParseInt(strings.TrimSpace(string(l)), 16, 64)
			if err != nil {
				c.close()
				return eProto
			}
			if _, err := c.br.Discard(int(n) + 2); err != nil {
				c.close()
				return classify(err, eRead)
			}
			if n == 0 {
				break
			}
		}
	case clen >= 0:
		if _, err := c.br.Discard(int(clen)); err != nil {
			c.close()
			return classify(err, eRead)
		}
	default:
		io.Copy(io.Discard, c.br)
		closeAfter = true
	}
	if closeAfter {
		c.close()
	}
	if status < 200 || status > 299 {
		return eStatus
	}
	return ""
}

type result struct {
	h    hist
	hjc  hist // open loop only: jitter-corrected latency (see below)
	late hist // open loop only: how late time.Sleep woke the sender (loadgen-side scheduling delay)
	errs map[errKind]uint64
}

func cpuTime() (float64, float64) {
	var ru syscall.Rusage
	syscall.Getrusage(syscall.RUSAGE_SELF, &ru)
	return float64(ru.Utime.Sec) + float64(ru.Utime.Usec)/1e6, float64(ru.Stime.Sec) + float64(ru.Stime.Usec)/1e6
}

// version is set at build time: -ldflags "-X main.version=..." (bench/workloads/build.sh).
var version = "dev"

func main() {
	addr := flag.String("addr", "127.0.0.1:8080", "host:port")
	path := flag.String("path", "/", "request path (with query)")
	method := flag.String("method", "GET", "HTTP method")
	body := flag.String("body", "", "request body")
	conns := flag.Int("conns", 16, "keep-alive connections (closed-loop concurrency / open-loop max in flight)")
	rate := flag.Float64("rate", 0, "open-loop target req/s (0 = closed loop)")
	dur := flag.Duration("duration", 10*time.Second, "measured duration")
	warm := flag.Duration("warmup", 2*time.Second, "warmup (not recorded)")
	timeout := flag.Duration("timeout", 5*time.Second, "per-request timeout")
	label := flag.String("label", "", "free-form label copied to output")
	showVersion := flag.Bool("version", false, "print the version and exit")
	flag.Parse()
	if *showVersion {
		fmt.Println("loadgen", version)
		return
	}

	var rb bytes.Buffer
	fmt.Fprintf(&rb, "%s %s HTTP/1.1\r\nHost: %s\r\nUser-Agent: k10s-loadgen\r\nAccept: */*\r\n", *method, *path, *addr)
	if *body != "" || *method == "POST" || *method == "PUT" {
		fmt.Fprintf(&rb, "Content-Length: %d\r\nContent-Type: text/plain\r\n", len(*body))
	}
	rb.WriteString("\r\n")
	rb.WriteString(*body)
	reqBytes := rb.Bytes()

	t0 := time.Now()
	measureStart := t0.Add(*warm)
	end := measureStart.Add(*dur)
	var cpuU0, cpuS0 float64
	var seq atomic.Int64
	interval := time.Duration(0)
	if *rate > 0 {
		interval = time.Duration(float64(time.Second) / *rate)
	}

	res := make([]*result, *conns)
	var wg sync.WaitGroup
	for i := 0; i < *conns; i++ {
		r := &result{errs: map[errKind]uint64{}}
		res[i] = r
		wg.Add(1)
		go func() {
			defer wg.Done()
			c := &client{addr: *addr, req: reqBytes, timeout: *timeout}
			defer c.close()
			for {
				var intended, start time.Time
				var late time.Duration
				if interval > 0 {
					intended = t0.Add(time.Duration(seq.Add(1)-1) * interval)
					if !intended.Before(end) {
						return
					}
					// Jitter-corrected start: when this sender was idle and slept until the intended time, a late
					// wake-up is the loadgen's own scheduling delay, so the corrected latency starts at the actual
					// wake-up; when it was behind schedule (its connection was busy: the system under test is slow),
					// it starts at the intended time, so queueing stays charged (no coordinated omission).
					if d := time.Until(intended); d > 0 {
						time.Sleep(d)
						start = time.Now()
						late = start.Sub(intended)
					} else {
						start = intended
					}
				} else {
					intended = time.Now()
					start = intended
					if !intended.Before(end) {
						return
					}
				}
				k := c.do()
				now := time.Now()
				lat := now.Sub(intended)
				if intended.Before(measureStart) {
					continue
				}
				if k != "" {
					r.errs[k]++
					if k != eStatus {
						// avoid a hot error loop on a dead endpoint
						time.Sleep(2 * time.Millisecond)
					}
					continue
				}
				r.h.add(uint64(lat / time.Microsecond))
				if interval > 0 {
					r.hjc.add(uint64(now.Sub(start) / time.Microsecond))
					r.late.add(uint64(late / time.Microsecond))
				}
			}
		}()
	}
	// snapshot CPU at the start of the measured window
	time.Sleep(time.Until(measureStart))
	cpuU0, cpuS0 = cpuTime()
	wg.Wait()
	cpuU1, cpuS1 := cpuTime()
	wall := time.Since(measureStart).Seconds()

	var h, hjc, hlate hist
	errs := map[string]uint64{}
	var nerr uint64
	for _, r := range res {
		h.merge(&r.h)
		hjc.merge(&r.hjc)
		hlate.merge(&r.late)
		for k, v := range r.errs {
			errs[string(k)] += v
			nerr += v
		}
	}
	out := map[string]any{
		"label": *label, "addr": *addr, "method": *method, "path": *path, "body_bytes": len(*body),
		"mode": map[bool]string{true: "open", false: "closed"}[*rate > 0], "conns": *conns,
		"target_rps": *rate, "duration_s": dur.Seconds(), "warmup_s": warm.Seconds(),
		"requests": h.n, "errors": nerr, "errors_by_kind": errs,
		"rps":           float64(h.n) / wall,
		"latency_us":    map[string]uint64{"p50": h.q(50), "p90": h.q(90), "p99": h.q(99), "p99.9": h.q(99.9), "max": h.max},
		"loadgen_cpu_s": map[string]float64{"user": cpuU1 - cpuU0, "sys": cpuS1 - cpuS0, "total": cpuU1 - cpuU0 + cpuS1 - cpuS0},
	}
	if *rate > 0 {
		out["latency_us_jc"] = map[string]uint64{"p50": hjc.q(50), "p90": hjc.q(90), "p99": hjc.q(99), "p99.9": hjc.q(99.9), "max": hjc.max}
		out["wakeup_late_us"] = map[string]uint64{"p50": hlate.q(50), "p99": hlate.q(99), "max": hlate.max}
	}
	_ = sort.Ints
	enc := json.NewEncoder(os.Stdout)
	enc.SetIndent("", " ")
	enc.Encode(out)
	if h.n == 0 {
		os.Exit(2)
	}
}
