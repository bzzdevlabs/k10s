// compute: GET /?n=N runs N rounds of sha256 chained, returns hex of the result.
package main

import (
	"crypto/sha256"
	"encoding/hex"
	"net/http"
	"os"
	"strconv"
)

func main() {
	port := os.Getenv("PORT")
	if port == "" {
		port = "8080"
	}
	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		n, _ := strconv.Atoi(r.URL.Query().Get("n"))
		if n <= 0 || n > 100_000_000 {
			n = 1000
		}
		h := sha256.Sum256([]byte("k10s"))
		for i := 0; i < n; i++ {
			h = sha256.Sum256(h[:])
		}
		w.Write([]byte(hex.EncodeToString(h[:]) + "\n"))
	})
	if err := http.ListenAndServe(":"+port, nil); err != nil {
		os.Exit(1)
	}
}
