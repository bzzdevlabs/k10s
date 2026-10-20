// chain: frontend; GET -> calls BACKEND_URL (default http://echo:8080/, i.e. the
// Kubernetes Service DNS name) over HTTP/1.1 keep-alive and returns its body.
package main

import (
	"io"
	"net/http"
	"os"
	"time"
)

// version is set at build time: -ldflags "-X main.version=..." (bench/workloads/build.sh).
var version = "dev"

func main() {
	if len(os.Args) > 1 && (os.Args[1] == "-version" || os.Args[1] == "--version") {
		os.Stdout.WriteString("chain " + version + "\n")
		return
	}
	port := os.Getenv("PORT")
	if port == "" {
		port = "8080"
	}
	backend := os.Getenv("BACKEND_URL")
	if backend == "" {
		backend = "http://echo:8080/"
	}
	cl := &http.Client{
		Timeout:   5 * time.Second,
		Transport: &http.Transport{MaxIdleConns: 256, MaxIdleConnsPerHost: 256, IdleConnTimeout: 90 * time.Second},
	}
	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		resp, err := cl.Get(backend)
		if err != nil {
			http.Error(w, err.Error(), http.StatusBadGateway)
			return
		}
		defer resp.Body.Close()
		if resp.StatusCode != 200 {
			http.Error(w, "backend status "+resp.Status, http.StatusBadGateway)
			return
		}
		io.Copy(w, resp.Body)
	})
	if err := http.ListenAndServe(":"+port, nil); err != nil {
		os.Exit(1)
	}
}
