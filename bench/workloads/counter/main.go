// counter: every request increments a counter persisted in COUNTER_FILE
// (default /data/counter) and returns the new value. Written with pwrite, no fsync.
package main

import (
	"net/http"
	"os"
	"strconv"
	"strings"
	"sync"
)

// version is set at build time: -ldflags "-X main.version=..." (bench/workloads/build.sh).
var version = "dev"

func main() {
	if len(os.Args) > 1 && (os.Args[1] == "-version" || os.Args[1] == "--version") {
		os.Stdout.WriteString("counter " + version + "\n")
		return
	}
	port := os.Getenv("PORT")
	if port == "" {
		port = "8080"
	}
	path := os.Getenv("COUNTER_FILE")
	if path == "" {
		path = "/data/counter"
	}
	f, err := os.OpenFile(path, os.O_RDWR|os.O_CREATE, 0o644)
	if err != nil {
		os.Stderr.WriteString("open: " + err.Error() + "\n")
		os.Exit(1)
	}
	var n uint64
	if b, _ := os.ReadFile(path); len(b) > 0 {
		n, _ = strconv.ParseUint(strings.TrimSpace(string(b)), 10, 64)
	}
	var mu sync.Mutex
	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		mu.Lock()
		n++
		s := strconv.FormatUint(n, 10) + "\n"
		f.Truncate(0)
		_, werr := f.WriteAt([]byte(s), 0)
		mu.Unlock()
		if werr != nil {
			http.Error(w, werr.Error(), 500)
			return
		}
		w.Write([]byte(s))
	})
	if err := http.ListenAndServe(":"+port, nil); err != nil {
		os.Exit(1)
	}
}
