// echo: the bench/workloads echo server in C, for scripts/oci-testimages.sh on hosts without Go. GET -> "hello",
// POST -> the body; HTTP/1.1 keep-alive, one process per connection; PORT (default 8080).
#define _GNU_SOURCE
#include <netinet/in.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <sys/socket.h>
#include <unistd.h>

static void serve(int c) {
    static char buf[65536];
    size_t have = 0;
    for (;;) {
        char *end = memmem(buf, have, "\r\n\r\n", 4);
        if (!end) {
            if (have == sizeof buf) return;
            ssize_t n = read(c, buf + have, sizeof buf - have);
            if (n <= 0) return;
            have += n;
            continue;
        }
        size_t head = end + 4 - buf, len = 0;
        *end = 0;
        char *cl = strcasestr(buf, "\r\ncontent-length:");
        if (cl) len = strtoul(cl + 17, NULL, 10);
        if (len > sizeof buf - head) return;
        while (have < head + len) {
            ssize_t n = read(c, buf + have, sizeof buf - have);
            if (n <= 0) return;
            have += n;
        }
        int post = !strncmp(buf, "POST ", 5);
        const char *body = post ? buf + head : "hello";
        size_t blen = post ? len : 5;
        char hdr[128];
        int h = snprintf(hdr, sizeof hdr, "HTTP/1.1 200 OK\r\nContent-Length: %zu\r\n\r\n", blen);
        if (write(c, hdr, h) != h || write(c, body, blen) != (ssize_t)blen) return;
        memmove(buf, buf + head + len, have - head - len);
        have -= head + len;
    }
}

int main(void) {
    const char *p = getenv("PORT");
    struct sockaddr_in6 a = {.sin6_family = AF_INET6, .sin6_port = htons(p ? atoi(p) : 8080)};
    int one = 1, l = socket(AF_INET6, SOCK_STREAM, 0);
    signal(SIGCHLD, SIG_IGN);
    setsockopt(l, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    if (bind(l, (void *)&a, sizeof a) || listen(l, 128)) return 1;
    for (;;) {
        int c = accept(l, NULL, NULL);
        if (c < 0) continue;
        if (fork() == 0) {
            serve(c);
            _exit(0);
        }
        close(c);
    }
}
