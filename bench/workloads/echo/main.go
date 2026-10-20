// echo: GET -> "hello", POST -> echoes the body. PORT env (default 8080).
package main

import (
	"io"
	"net/http"
	"os"
)

// version is set at build time: -ldflags "-X main.version=..." (bench/workloads/build.sh).
var version = "dev"

func main() {
	if len(os.Args) > 1 && (os.Args[1] == "-version" || os.Args[1] == "--version") {
		os.Stdout.WriteString("echo " + version + "\n")
		return
	}
	port := os.Getenv("PORT")
	if port == "" {
		port = "8080"
	}
	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		if r.Method == http.MethodPost {
			io.Copy(w, r.Body)
			return
		}
		w.Write([]byte("hello"))
	})
	if err := http.ListenAndServe(":"+port, nil); err != nil {
		os.Exit(1)
	}
}
