// lxprobe: deterministic probes of the Linux calls k10-lx implements in M1 (docs/LX.md). Every line is
// "ok GROUP NAME[ = VALUE]" or "FAIL GROUP NAME: DETAIL", with no pid, address, port or time in it, so the output
// of one binary under Linux and under k10s compares byte for byte (oci.md §13). Built static and dynamic against
// musl by scripts/oci-testimages.sh.  usage: lxprobe [GROUP...]   exit status: the number of failures
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <ifaddrs.h>
#include <net/if.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <spawn.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/file.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/random.h>
#include <sys/resource.h>
#include <sys/select.h>
#include <sys/sendfile.h>
#include <sys/signalfd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/timerfd.h>
#include <sys/uio.h>
#include <sys/un.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef MAP_FIXED_NOREPLACE
#define MAP_FIXED_NOREPLACE 0x100000
#endif
#ifndef RENAME_NOREPLACE
#define RENAME_NOREPLACE 1
#endif

extern char **environ;
static const char *group;
static int fails;
static char self[4096];
static char tmp[64];
static const char *argv0;

static const char *errname(int e) {
    static const struct { int n; const char *s; } t[] = {
        {0, "0"}, {EPERM, "EPERM"}, {ENOENT, "ENOENT"}, {ESRCH, "ESRCH"}, {EINTR, "EINTR"}, {EBADF, "EBADF"},
        {ECHILD, "ECHILD"}, {EAGAIN, "EAGAIN"}, {ENOMEM, "ENOMEM"}, {EACCES, "EACCES"}, {EFAULT, "EFAULT"},
        {EEXIST, "EEXIST"}, {ENOTDIR, "ENOTDIR"}, {EISDIR, "EISDIR"}, {EINVAL, "EINVAL"}, {EMFILE, "EMFILE"},
        {ENOTTY, "ENOTTY"}, {ESPIPE, "ESPIPE"}, {EPIPE, "EPIPE"}, {ENOSYS, "ENOSYS"}, {ENOTEMPTY, "ENOTEMPTY"},
        {ELOOP, "ELOOP"}, {ENOTSOCK, "ENOTSOCK"}, {EOPNOTSUPP, "EOPNOTSUPP"}, {EAFNOSUPPORT, "EAFNOSUPPORT"},
        {EADDRINUSE, "EADDRINUSE"}, {ECONNREFUSED, "ECONNREFUSED"}, {EINPROGRESS, "EINPROGRESS"},
        {ENOTCONN, "ENOTCONN"}, {ETIMEDOUT, "ETIMEDOUT"}, {EXDEV, "EXDEV"}, {ENODATA, "ENODATA"}};
    static char buf[16];
    for (size_t i = 0; i < sizeof t / sizeof t[0]; i++)
        if (t[i].n == e) return t[i].s;
    snprintf(buf, sizeof buf, "E%d", e);
    return buf;
}

static void check(const char *name, int cond, const char *fmt, ...) {
    int e = errno;
    if (cond) {
        printf("ok %s %s\n", group, name);
    } else {
        va_list ap;
        printf("FAIL %s %s: ", group, name);
        va_start(ap, fmt);
        vprintf(fmt, ap);
        va_end(ap);
        printf(" (errno %s)\n", errname(e));
        fails++;
    }
    fflush(stdout);
}

static void value(const char *name, const char *fmt, ...) {
    va_list ap;
    printf("ok %s %s = ", group, name);
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    printf("\n");
    fflush(stdout);
}

// The errno of a call expected to fail, as a name.
#define ERR(call) ((call) == -1 ? errname(errno) : "success")

static int status_of(pid_t pid) {
    int st = 0;
    if (waitpid(pid, &st, 0) != pid) return -1;
    return WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
}

static int64_t now_ns(clockid_t c) {
    struct timespec ts;
    clock_gettime(c, &ts);
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

static void path(char *out, size_t n, const char *name) { snprintf(out, n, "%s/%s", tmp, name); }

// ---- proc ----

static void *thread_main(void *arg) {
    *(pid_t *)arg = gettid();
    return (void *)42;
}

static void *thread_exit(void *arg) {
    (void)arg;
    exit(3);
}

static void proc(void) {
    group = "proc";
    check("gettid-is-pid", gettid() == getpid(), "tid differs");
    pid_t c = fork();
    if (c == 0) _exit(42);
    check("fork-wait", status_of(c) == 42, "status");
    c = vfork();
    if (c == 0) _exit(7);
    check("vfork-wait", status_of(c) == 7, "status");
    c = fork();
    if (c == 0) {
        char *argv[] = {self, "exec-child", "a", "b", NULL};
        execve(self, argv, environ);
        _exit(99);
    }
    check("execve", status_of(c) == 4, "exit status is not argc");
    char *argv[] = {self, "exec-child", NULL};
    check("posix_spawn", posix_spawn(&c, self, NULL, NULL, argv, environ) == 0 && status_of(c) == 2, "spawn");
    pthread_t t;
    pid_t tid = 0;
    void *ret = NULL;
    check("thread", pthread_create(&t, NULL, thread_main, &tid) == 0 && pthread_join(t, &ret) == 0 &&
                        ret == (void *)42 && tid != getpid() && tid > 0, "create/join");
    c = fork();
    if (c == 0) {
        pthread_create(&t, NULL, thread_exit, NULL);
        for (;;) pause();
    }
    check("exit-group-from-thread", status_of(c) == 3, "status");
    int st;
    check("wait-no-child", waitpid(-1, &st, WNOHANG) == -1 && errno == ECHILD, "%s", errname(errno));
    c = fork();
    if (c == 0) _exit(setpgid(0, 0) == 0 && getpgid(0) == getpid() ? 0 : 1);
    check("setpgid", status_of(c) == 0, "child");
    c = fork();
    if (c == 0) _exit(setsid() == getpid() && getsid(0) == getpid() ? 0 : 1);
    check("setsid", status_of(c) == 0, "child");
    c = fork();
    if (c == 0)
        for (;;) pause();
    kill(c, SIGTERM);
    check("kill-term", status_of(c) == 128 + SIGTERM, "status");
    struct utsname u;
    uname(&u);
    value("uname", "%s %s", u.sysname, u.machine);
    struct rlimit r, r2;
    getrlimit(RLIMIT_NOFILE, &r);
    r2 = r;
    r2.rlim_cur = r.rlim_cur > 64 ? 64 : r.rlim_cur;
    check("rlimit-nofile", setrlimit(RLIMIT_NOFILE, &r2) == 0 && getrlimit(RLIMIT_NOFILE, &r2) == 0 &&
                               r2.rlim_cur == (r.rlim_cur > 64 ? 64 : r.rlim_cur) && setrlimit(RLIMIT_NOFILE, &r) == 0,
          "set/get");
    char name[16] = {0};
    check("prctl-name", prctl(PR_SET_NAME, "lxprobe-t") == 0 && prctl(PR_GET_NAME, name) == 0 &&
                            strcmp(name, "lxprobe-t") == 0, "name %s", name);
    prctl(PR_SET_NAME, "lxprobe");
    struct rusage ru;
    check("getrusage", getrusage(RUSAGE_SELF, &ru) == 0 && getrusage(RUSAGE_CHILDREN, &ru) == 0, "call");
    check("affinity", sysconf(_SC_NPROCESSORS_ONLN) >= 1, "cpus");
}

// ---- mem ----

static sigjmp_buf jb;
static volatile int fault_code;
static volatile void *fault_addr;

static void on_segv(int sig, siginfo_t *si, void *uc) {
    (void)sig;
    (void)uc;
    fault_code = si->si_code;
    fault_addr = si->si_addr;
    siglongjmp(jb, 1);
}

static int faults(volatile char *p, int write) {
    fault_code = 0;
    if (sigsetjmp(jb, 1) == 0) {
        if (write)
            *p = 1;
        else
            (void)*p;
        return 0;
    }
    return fault_addr == p ? fault_code : -1;
}

static void mem(void) {
    group = "mem";
    long pg = sysconf(_SC_PAGESIZE);
    value("page-size", "%ld", pg);
    struct sigaction sa = {.sa_sigaction = on_segv, .sa_flags = SA_SIGINFO};
    sigaction(SIGSEGV, &sa, NULL);
    char *p = mmap(NULL, 4 * pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check("mmap-anon", p != MAP_FAILED && p[0] == 0 && p[4 * pg - 1] == 0, "map");
    memset(p, 0x5a, 4 * pg);
    check("mprotect-read-only", mprotect(p, pg, PROT_READ) == 0 && faults(p, 1) == SEGV_ACCERR && p[0] == 0x5a,
          "code %d", fault_code);
    check("mmap-fixed-noreplace", mmap(p, pg, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0) ==
                                          MAP_FAILED && errno == EEXIST, "%s", errname(errno));
    check("madvise-dontneed", madvise(p + pg, pg, MADV_DONTNEED) == 0 && p[pg] == 0 && p[2 * pg] == 0x5a, "zeroed");
    mprotect(p, pg, PROT_READ | PROT_WRITE);
    char *q = mremap(p, 4 * pg, 64 * pg, MREMAP_MAYMOVE);
    check("mremap-grow", q != MAP_FAILED && q[2 * pg] == 0x5a && q[63 * pg] == 0, "content");
    check("munmap-maperr", munmap(q, 64 * pg) == 0 && faults(q, 0) == SEGV_MAPERR, "code %d", fault_code);
    char *big = mmap(NULL, 1L << 30, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    check("reserve-1g", big != MAP_FAILED && mprotect(big + (512L << 20), pg, PROT_READ | PROT_WRITE) == 0 &&
                            (big[512L << 20] = 1) == 1 && faults(big, 0) == SEGV_ACCERR, "reserve");
    munmap(big, 1L << 30);
    uintptr_t b0 = (uintptr_t)syscall(SYS_brk, 0);
    uintptr_t b1 = (uintptr_t)syscall(SYS_brk, b0 + 8 * pg);
    check("brk", b1 == b0 + 8 * pg && (((char *)b0)[pg] = 3) == 3 && (uintptr_t)syscall(SYS_brk, b0) == b0, "break");
    char f[96];
    path(f, sizeof f, "map");
    int fd = open(f, O_RDWR | O_CREAT | O_TRUNC, 0600);
    ftruncate(fd, pg);
    char *s = mmap(NULL, pg, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    memcpy(s, "shared", 6);
    char buf[8] = {0};
    check("map-shared-file", msync(s, pg, MS_SYNC) == 0 && pread(fd, buf, 6, 0) == 6 && !memcmp(buf, "shared", 6),
          "file %s", buf);
    char *v = mmap(NULL, pg, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    v[0] = 'X';
    check("map-private-cow", pread(fd, buf, 1, 0) == 1 && buf[0] == 's' && s[0] == 's', "file changed");
    unsigned char inc;
    check("mincore", mincore(s, pg, &inc) == 0, "call");
    munmap(s, pg);
    munmap(v, pg);
    close(fd);
    char *w = mmap(NULL, pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    w[0] = 1;
    pid_t c = fork();
    if (c == 0) _exit(w[0] == 1 && (w[0] = 5) == 5 ? 0 : 1);
    check("fork-cow", status_of(c) == 0 && w[0] == 1, "parent sees %d", w[0]);
    munmap(w, pg);
    unlink(f);
    signal(SIGSEGV, SIG_DFL);
}

// ---- sig ----

static volatile int got, got_code, on_alt;
static stack_t alt;

static void on_sig(int sig, siginfo_t *si, void *uc) {
    (void)uc;
    char here;
    got = sig;
    got_code = si->si_code;
    on_alt = &here >= (char *)alt.ss_sp && &here < (char *)alt.ss_sp + alt.ss_size;
}

static void sig(void) {
    group = "sig";
    struct sigaction sa = {.sa_sigaction = on_sig, .sa_flags = SA_SIGINFO};
    sigaction(SIGUSR1, &sa, NULL);
    got = 0;
    raise(SIGUSR1);
    check("raise", got == SIGUSR1 && got_code == SI_TKILL, "got %d code %d", got, got_code);
    sigset_t m, old, pend;
    sigemptyset(&m);
    sigaddset(&m, SIGUSR1);
    sigprocmask(SIG_BLOCK, &m, &old);
    got = 0;
    kill(getpid(), SIGUSR1);
    sigpending(&pend);
    check("blocked-pending", got == 0 && sigismember(&pend, SIGUSR1), "delivered while blocked");
    sigsuspend(&old);
    check("sigsuspend", got == SIGUSR1 && got_code == SI_USER, "got %d", got);
    sigprocmask(SIG_SETMASK, &old, NULL);
    alt.ss_size = 64 * 1024;
    alt.ss_sp = malloc(alt.ss_size);
    sigaltstack(&alt, NULL);
    sa.sa_flags = SA_SIGINFO | SA_ONSTACK;
    sigaction(SIGUSR2, &sa, NULL);
    raise(SIGUSR2);
    check("sigaltstack", got == SIGUSR2 && on_alt, "alt %d", on_alt);
    sigemptyset(&m);
    sigaddset(&m, SIGUSR2);
    sigprocmask(SIG_BLOCK, &m, NULL);
    raise(SIGUSR2);
    siginfo_t si;
    struct timespec ts = {1, 0};
    check("sigtimedwait", sigtimedwait(&m, &si, &ts) == SIGUSR2 && si.si_code == SI_TKILL, "wait");
    sigprocmask(SIG_UNBLOCK, &m, NULL);
    int p[2];
    pipe(p);
    close(p[0]);
    signal(SIGPIPE, SIG_IGN);
    check("epipe", write(p[1], "x", 1) == -1 && errno == EPIPE, "%s", errname(errno));
    signal(SIGPIPE, SIG_DFL);
    pid_t c = fork();
    if (c == 0) {
        write(p[1], "x", 1);
        _exit(0);
    }
    check("sigpipe-kills", status_of(c) == 128 + SIGPIPE, "status");
    close(p[1]);
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGALRM, &sa, NULL);
    pipe(p);
    struct itimerval it = {{0, 20000}, {0, 20000}}, off = {{0, 0}, {0, 0}};
    got = 0;
    setitimer(ITIMER_REAL, &it, NULL);
    char b;
    check("eintr", read(p[0], &b, 1) == -1 && errno == EINTR && got == SIGALRM, "%s", errname(errno));
    setitimer(ITIMER_REAL, &off, NULL);
    sa.sa_flags = SA_SIGINFO | SA_RESTART;
    sigaction(SIGALRM, &sa, NULL);
    got = 0;
    c = fork();
    if (c == 0) {
        kill(getppid(), SIGALRM);
        usleep(20000);
        write(p[1], "r", 1);
        _exit(0);
    }
    check("sa-restart", read(p[0], &b, 1) == 1 && b == 'r' && got == SIGALRM, "%s", errname(errno));
    status_of(c);
    close(p[0]);
    close(p[1]);
    timer_t tm;
    struct sigevent ev = {.sigev_notify = SIGEV_SIGNAL, .sigev_signo = SIGUSR1};
    struct itimerspec its = {{0, 0}, {0, 5000000}};
    sigemptyset(&m);
    sigaddset(&m, SIGUSR1);
    sigprocmask(SIG_BLOCK, &m, NULL);
    check("timer-create", timer_create(CLOCK_MONOTONIC, &ev, &tm) == 0 && timer_settime(tm, 0, &its, NULL) == 0 &&
                              sigwaitinfo(&m, &si) == SIGUSR1 && si.si_code == SI_TIMER, "timer");
    timer_delete(tm);
    sigprocmask(SIG_UNBLOCK, &m, NULL);
    signal(SIGCHLD, SIG_DFL);
}

// ---- time ----

static void timeprobe(void) {
    group = "time";
    int64_t a = now_ns(CLOCK_MONOTONIC);
    struct timespec ts = {0, 2000000};
    nanosleep(&ts, NULL);
    int64_t b = now_ns(CLOCK_MONOTONIC);
    check("nanosleep", b - a >= 2000000, "slept %lld ns", (long long)(b - a));
    check("realtime", now_ns(CLOCK_REALTIME) > 1700000000LL * 1000000000, "clock before 2023");
    struct timeval tv;
    gettimeofday(&tv, NULL);
    check("gettimeofday", llabs((long long)tv.tv_sec - now_ns(CLOCK_REALTIME) / 1000000000) <= 1, "skew");
    struct timespec res;
    check("getres", clock_getres(CLOCK_MONOTONIC, &res) == 0 && res.tv_sec == 0 && res.tv_nsec <= 1000000, "res");
    check("cputime", now_ns(CLOCK_PROCESS_CPUTIME_ID) > 0 && now_ns(CLOCK_THREAD_CPUTIME_ID) > 0, "zero");
    struct timespec until;
    clock_gettime(CLOCK_MONOTONIC, &until);
    until.tv_nsec += 1000000;
    if (until.tv_nsec >= 1000000000) {
        until.tv_sec++;
        until.tv_nsec -= 1000000000;
    }
    check("clock-nanosleep-abs", clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &until, NULL) == 0 &&
                                     now_ns(CLOCK_MONOTONIC) >= (int64_t)until.tv_sec * 1000000000 + until.tv_nsec,
          "early");
    unsigned char r[32] = {0};
    int nz = 0;
    check("getrandom", getrandom(r, sizeof r, GRND_NONBLOCK) == sizeof r, "short");
    for (size_t i = 0; i < sizeof r; i++) nz += r[i] != 0;
    check("getrandom-bytes", nz > 16, "%d non-zero", nz);
}

// ---- file ----

static int cmpstr(const void *a, const void *b) { return strcmp(*(char *const *)a, *(char *const *)b); }

static void file(void) {
    group = "file";
    char a[96], b[96], d[96], l[96];
    path(a, sizeof a, "a");
    path(b, sizeof b, "b");
    path(d, sizeof d, "d");
    path(l, sizeof l, "l");
    mode_t old = umask(022);
    int fd = open(a, O_RDWR | O_CREAT | O_EXCL, 0666);
    check("create-excl", fd >= 0 && open(a, O_RDWR | O_CREAT | O_EXCL, 0666) == -1 && errno == EEXIST, "%s",
          errname(errno));
    struct stat st;
    fstat(fd, &st);
    value("umask-mode", "%o", st.st_mode & 0777);
    char buf[64] = {0};
    check("write-pread", write(fd, "0123456789", 10) == 10 && lseek(fd, 0, SEEK_END) == 10 &&
                             pread(fd, buf, 3, 2) == 3 && !memcmp(buf, "234", 3), "data");
    check("pwrite-hole", pwrite(fd, "z", 1, 20) == 1 && fstat(fd, &st) == 0 && st.st_size == 21, "size");
    check("ftruncate", ftruncate(fd, 5) == 0 && fstat(fd, &st) == 0 && st.st_size == 5, "size");
    struct iovec iov[2] = {{"ab", 2}, {"cd", 2}};
    lseek(fd, 0, SEEK_SET);
    check("writev-readv", writev(fd, iov, 2) == 4 && lseek(fd, 0, SEEK_SET) == 0 &&
                              readv(fd, (struct iovec[]){{buf, 4}}, 1) == 4 && !memcmp(buf, "abcd", 4), "data");
    check("fsync", fsync(fd) == 0 && fdatasync(fd) == 0, "sync");
    check("fallocate", fallocate(fd, 0, 0, 8192) == 0 && fstat(fd, &st) == 0 && st.st_size == 8192, "size");
    close(fd);
    check("rename", rename(a, b) == 0 && access(a, F_OK) == -1 && errno == ENOENT && access(b, R_OK) == 0, "rename");
    close(open(a, O_CREAT | O_WRONLY, 0644));
    check("renameat2-noreplace", syscall(SYS_renameat2, AT_FDCWD, a, AT_FDCWD, b, RENAME_NOREPLACE) == -1 &&
                                     errno == EEXIST, "%s", errname(errno));
    check("link", link(b, l) == 0 && stat(b, &st) == 0 && st.st_nlink == 2 && unlink(l) == 0 && stat(b, &st) == 0 &&
                      st.st_nlink == 1, "nlink");
    check("symlink", symlink("b", l) == 0 && readlink(l, buf, sizeof buf) == 1 && buf[0] == 'b' &&
                         lstat(l, &st) == 0 && S_ISLNK(st.st_mode), "readlink");
    check("nofollow", open(l, O_RDONLY | O_NOFOLLOW) == -1 && errno == ELOOP, "%s", errname(errno));
    check("mkdir", mkdir(d, 0755) == 0 && mkdir(d, 0755) == -1 && errno == EEXIST, "%s", errname(errno));
    char sub[128];
    snprintf(sub, sizeof sub, "%s/x", d);
    close(open(sub, O_CREAT | O_WRONLY, 0644));
    check("rmdir-notempty", rmdir(d) == -1 && errno == ENOTEMPTY, "%s", errname(errno));
    check("open-errors", open(b, O_RDONLY | O_DIRECTORY) == -1 && errno == ENOTDIR &&
                             open(d, O_WRONLY) == -1 && errno == EISDIR &&
                             open(d, O_RDONLY | O_CREAT | O_EXCL, 0600) == -1 && errno == EEXIST, "%s", errname(errno));
    DIR *dir = opendir(tmp);
    char *names[16];
    int n = 0;
    struct dirent *de;
    while (dir && (de = readdir(dir)) && n < 16) names[n++] = strdup(de->d_name);
    closedir(dir);
    qsort(names, n, sizeof names[0], cmpstr);
    char list[128] = "";
    for (int i = 0; i < n; i++) {
        strcat(list, names[i]);
        strcat(list, i + 1 < n ? " " : "");
        free(names[i]);
    }
    value("getdents", "%s", list);
    fd = open(b, O_WRONLY);
    check("append", fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) | O_APPEND) == 0 && (fcntl(fd, F_GETFL) & O_APPEND) &&
                        write(fd, "!", 1) == 1 && stat(b, &st) == 0 && st.st_size == 8193, "size");
    check("cloexec", fcntl(fd, F_SETFD, FD_CLOEXEC) == 0 && fcntl(fd, F_GETFD) == FD_CLOEXEC, "flag");
    int e = dup3(fd, 40, O_CLOEXEC);
    check("dup3", e == 40 && fcntl(40, F_GETFD) == FD_CLOEXEC && dup2(40, 41) == 41 && fcntl(41, F_GETFD) == 0,
          "flags");
    close(40);
    pid_t c = fork();
    if (c == 0) {
        char fdarg[8];
        snprintf(fdarg, sizeof fdarg, "%d", fd);
        char *argv[] = {self, "exec-child", "check-closed", fdarg, "41", NULL};
        execve(self, argv, environ);
        _exit(99);
    }
    check("cloexec-on-exec", status_of(c) == 0, "fd survived exec");
    close(41);
    close(fd);
    fd = open(b, O_RDWR);
    struct flock fl = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 10};
    check("fcntl-lock", fcntl(fd, F_SETLK, &fl) == 0, "set");
    c = fork();
    if (c == 0) {
        int f2 = open(b, O_RDWR);
        struct flock q = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 10};
        int busy = fcntl(f2, F_SETLK, &q) == -1 && (errno == EAGAIN || errno == EACCES);
        q.l_type = F_WRLCK;
        fcntl(f2, F_GETLK, &q);
        _exit(busy && q.l_type == F_WRLCK && q.l_pid == getppid() ? 0 : 1);
    }
    check("fcntl-lock-conflict", status_of(c) == 0, "child");
    int f3 = open(b, O_RDONLY);
    check("flock", flock(fd, LOCK_EX) == 0 && flock(f3, LOCK_EX | LOCK_NB) == -1 && errno == EWOULDBLOCK &&
                       flock(fd, LOCK_UN) == 0 && flock(f3, LOCK_SH | LOCK_NB) == 0, "%s", errname(errno));
    close(f3);
    close(fd);
    check("chmod", chmod(b, 0600) == 0 && stat(b, &st) == 0 && (st.st_mode & 0777) == 0600, "mode");
    struct timespec times[2] = {{1000000000, 0}, {1000000000, 500}};
    check("utimensat", utimensat(AT_FDCWD, b, times, 0) == 0 && stat(b, &st) == 0 &&
                           st.st_mtim.tv_sec == 1000000000 && st.st_mtim.tv_nsec == 500, "mtime");
    struct statx sx;
    check("statx", statx(AT_FDCWD, b, 0, STATX_SIZE | STATX_MODE, &sx) == 0 && (sx.stx_mask & STATX_SIZE) &&
                       sx.stx_size == 8193, "size");
    check("truncate", truncate(b, 1) == 0 && stat(b, &st) == 0 && st.st_size == 1, "size");
    char cwd[256], back[256];
    getcwd(cwd, sizeof cwd);
    int dfd = open(".", O_RDONLY | O_DIRECTORY);
    check("chdir", chdir(d) == 0 && getcwd(back, sizeof back) && !strcmp(back, d) && fchdir(dfd) == 0 &&
                       getcwd(back, sizeof back) && !strcmp(back, cwd), "cwd");
    close(dfd);
    int m = memfd_create("lxprobe", MFD_CLOEXEC);
    char *mp;
    check("memfd", m >= 0 && write(m, "memfd", 5) == 5 && fstat(m, &st) == 0 && st.st_size == 5 &&
                       (mp = mmap(NULL, 5, PROT_READ, MAP_SHARED, m, 0)) != MAP_FAILED && !memcmp(mp, "memfd", 5),
          "memfd");
    int p[2];
    pipe(p);
    fd = open(b, O_RDWR | O_TRUNC);
    write(fd, "sendfile", 8);
    off_t off = 0;
    check("sendfile", sendfile(p[1], fd, &off, 8) == 8 && read(p[0], buf, 8) == 8 && !memcmp(buf, "sendfile", 8),
          "data");
    int g = open(a, O_RDWR | O_TRUNC);
    loff_t in = 0, out = 0;
    check("copy-file-range", copy_file_range(fd, &in, g, &out, 8, 0) == 8 && pread(g, buf, 8, 0) == 8 &&
                                 !memcmp(buf, "sendfile", 8), "data");
    close(g);
    close(fd);
    close(p[0]);
    close(p[1]);
    unlink(sub);
    rmdir(d);
    unlink(l);
    unlink(a);
    unlink(b);
    umask(old);
}

// ---- pipe ----

static void pipes(void) {
    group = "pipe";
    int p[2];
    char b[8];
    check("pipe2", pipe2(p, O_CLOEXEC | O_NONBLOCK) == 0 && fcntl(p[0], F_GETFD) == FD_CLOEXEC, "flags");
    check("empty-eagain", read(p[0], b, 1) == -1 && errno == EAGAIN, "%s", errname(errno));
    long cap = 0;
    static char chunk[4096];
    while (write(p[1], chunk, sizeof chunk) > 0) cap += sizeof chunk;
    value("capacity", "%ld", cap);
    value("getpipe-sz", "%d", fcntl(p[1], F_GETPIPE_SZ));
    while (read(p[0], chunk, sizeof chunk) > 0) {
    }
    struct pollfd pf = {.fd = p[0], .events = POLLIN};
    check("poll-empty", poll(&pf, 1, 0) == 0, "ready");
    write(p[1], "x", 1);
    check("poll-in", poll(&pf, 1, 0) == 1 && pf.revents == POLLIN, "revents %x", pf.revents);
    fd_set rs;
    FD_ZERO(&rs);
    FD_SET(p[0], &rs);
    struct timeval tv = {0, 0};
    check("select", select(p[0] + 1, &rs, NULL, NULL, &tv) == 1 && FD_ISSET(p[0], &rs), "ready");
    read(p[0], b, 1);
    close(p[1]);
    check("poll-hup", poll(&pf, 1, 0) == 1 && (pf.revents & POLLHUP) && read(p[0], b, 1) == 0, "revents %x",
          pf.revents);
    close(p[0]);
    pipe2(p, O_NONBLOCK);
    int ep = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event ev = {.events = EPOLLIN | EPOLLET, .data.u32 = 7}, out[4];
    check("epoll-add", epoll_ctl(ep, EPOLL_CTL_ADD, p[0], &ev) == 0 && epoll_wait(ep, out, 4, 0) == 0, "add");
    write(p[1], "ab", 2);
    check("epoll-in", epoll_wait(ep, out, 4, 0) == 1 && out[0].data.u32 == 7 && out[0].events == EPOLLIN,
          "events %x", out[0].events);
    check("epoll-edge", epoll_wait(ep, out, 4, 0) == 0, "re-armed");
    check("epoll-dup-add", epoll_ctl(ep, EPOLL_CTL_ADD, p[0], &ev) == -1 && errno == EEXIST, "%s", errname(errno));
    ev.events = EPOLLIN;
    check("epoll-mod", epoll_ctl(ep, EPOLL_CTL_MOD, p[0], &ev) == 0 && epoll_wait(ep, out, 4, 0) == 1, "level");
    close(p[1]);
    check("epoll-hup", epoll_wait(ep, out, 4, 0) == 1 && (out[0].events & EPOLLHUP), "events %x", out[0].events);
    check("epoll-del", epoll_ctl(ep, EPOLL_CTL_DEL, p[0], NULL) == 0 && epoll_wait(ep, out, 4, 0) == 0, "del");
    close(p[0]);
    close(ep);
}

// ---- event ----

static void events(void) {
    group = "event";
    int e = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    uint64_t v = 3, r = 0;
    write(e, &v, 8);
    v = 4;
    write(e, &v, 8);
    check("eventfd-sum", read(e, &r, 8) == 8 && r == 7, "read %llu", (unsigned long long)r);
    check("eventfd-empty", read(e, &r, 8) == -1 && errno == EAGAIN, "%s", errname(errno));
    close(e);
    e = eventfd(2, EFD_SEMAPHORE);
    check("eventfd-semaphore", read(e, &r, 8) == 8 && r == 1 && read(e, &r, 8) == 8 && r == 1, "read");
    close(e);
    int t = timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC);
    struct itimerspec its = {{0, 0}, {0, 5000000}};
    timerfd_settime(t, 0, &its, NULL);
    check("timerfd-oneshot", read(t, &r, 8) == 8 && r == 1, "expirations %llu", (unsigned long long)r);
    its.it_interval.tv_nsec = 1000000;
    its.it_value.tv_nsec = 1000000;
    timerfd_settime(t, 0, &its, NULL);
    struct timespec ts = {0, 10000000};
    nanosleep(&ts, NULL);
    check("timerfd-periodic", read(t, &r, 8) == 8 && r >= 5, "expirations %llu", (unsigned long long)r);
    close(t);
    sigset_t m;
    sigemptyset(&m);
    sigaddset(&m, SIGUSR2);
    sigprocmask(SIG_BLOCK, &m, NULL);
    int s = signalfd(-1, &m, SFD_CLOEXEC);
    raise(SIGUSR2);
    struct signalfd_siginfo si;
    check("signalfd", read(s, &si, sizeof si) == sizeof si && si.ssi_signo == SIGUSR2, "signo %u", si.ssi_signo);
    close(s);
    sigprocmask(SIG_UNBLOCK, &m, NULL);
}

// ---- sock ----

static void sock(void) {
    group = "sock";
    int sv[2];
    char b[256] = {0};
    check("socketpair-stream", socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sv) == 0 &&
                                   send(sv[0], "hi", 2, 0) == 2 && recv(sv[1], b, sizeof b, 0) == 2, "data");
    struct ucred uc;
    socklen_t ul = sizeof uc;
    check("peercred", getsockopt(sv[0], SOL_SOCKET, SO_PEERCRED, &uc, &ul) == 0 && uc.pid == getpid() &&
                          uc.uid == getuid(), "cred");
    int p[2];
    pipe(p);
    char cm[CMSG_SPACE(sizeof(int))] = {0};
    struct iovec io = {"f", 1};
    struct msghdr mh = {.msg_iov = &io, .msg_iovlen = 1, .msg_control = cm, .msg_controllen = sizeof cm};
    struct cmsghdr *c = CMSG_FIRSTHDR(&mh);
    c->cmsg_level = SOL_SOCKET;
    c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c), &p[1], sizeof(int));
    sendmsg(sv[0], &mh, 0);
    memset(cm, 0, sizeof cm);
    io.iov_base = b;
    int got = -1;
    if (recvmsg(sv[1], &mh, MSG_CMSG_CLOEXEC) == 1 && (c = CMSG_FIRSTHDR(&mh)) && c->cmsg_type == SCM_RIGHTS)
        memcpy(&got, CMSG_DATA(c), sizeof(int));
    check("scm-rights", got >= 0 && got != p[1] && fcntl(got, F_GETFD) == FD_CLOEXEC && write(got, "z", 1) == 1 &&
                            read(p[0], b, 1) == 1 && b[0] == 'z', "fd");
    close(got);
    close(p[0]);
    close(p[1]);
    shutdown(sv[0], SHUT_WR);
    check("shutdown-eof", recv(sv[1], b, sizeof b, 0) == 0, "data after shutdown");
    close(sv[0]);
    close(sv[1]);
    socketpair(AF_UNIX, SOCK_DGRAM, 0, sv);
    send(sv[0], "one", 3, 0);
    send(sv[0], "three", 5, 0);
    check("dgram-boundaries", recv(sv[1], b, sizeof b, 0) == 3 && recv(sv[1], b, sizeof b, 0) == 5, "sizes");
    close(sv[0]);
    close(sv[1]);
    socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sv);
    send(sv[0], "abcdef", 6, 0);
    check("seqpacket-truncates", recv(sv[1], b, 2, 0) == 2 && recv(sv[1], b, sizeof b, MSG_DONTWAIT) == -1 &&
                                     errno == EAGAIN, "%s", errname(errno));
    close(sv[0]);
    close(sv[1]);
    struct sockaddr_un un = {.sun_family = AF_UNIX};
    path(un.sun_path, sizeof un.sun_path, "sock");
    int l = socket(AF_UNIX, SOCK_STREAM, 0);
    int cl = socket(AF_UNIX, SOCK_STREAM, 0);
    check("unix-path", bind(l, (void *)&un, sizeof un) == 0 && listen(l, 4) == 0 &&
                           connect(cl, (void *)&un, sizeof un) == 0, "connect");
    int ac = accept4(l, NULL, NULL, SOCK_CLOEXEC);
    check("unix-accept", ac >= 0 && write(cl, "u", 1) == 1 && read(ac, b, 1) == 1, "accept");
    check("unix-addrinuse", bind(socket(AF_UNIX, SOCK_STREAM, 0), (void *)&un, sizeof un) == -1 &&
                                errno == EADDRINUSE, "%s", errname(errno));
    close(ac);
    close(cl);
    close(l);
    unlink(un.sun_path);
    struct sockaddr_in sa = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t sl = sizeof sa;
    int one = 1, v = 0;
    l = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
    setsockopt(l, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    check("tcp-listen", bind(l, (void *)&sa, sizeof sa) == 0 && getsockname(l, (void *)&sa, &sl) == 0 &&
                            sa.sin_port != 0 && listen(l, 16) == 0, "listen");
    cl = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    int r = connect(cl, (void *)&sa, sizeof sa);
    check("tcp-connect-nonblock", r == 0 || errno == EINPROGRESS, "%s", errname(errno));
    int ep = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLOUT}, out;
    epoll_ctl(ep, EPOLL_CTL_ADD, cl, &ev);
    socklen_t vl = sizeof v;
    check("tcp-connected", epoll_wait(ep, &out, 1, 5000) == 1 && getsockopt(cl, SOL_SOCKET, SO_ERROR, &v, &vl) == 0 &&
                               v == 0, "so_error %d", v);
    close(ep);
    struct sockaddr_in peer;
    sl = sizeof peer;
    ac = accept4(l, (void *)&peer, &sl, SOCK_CLOEXEC);
    check("tcp-accept", ac >= 0 && peer.sin_family == AF_INET && peer.sin_addr.s_addr == htonl(INADDR_LOOPBACK),
          "peer");
    setsockopt(ac, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
    vl = sizeof v;
    check("tcp-nodelay", getsockopt(ac, IPPROTO_TCP, TCP_NODELAY, &v, &vl) == 0 && v != 0, "value %d", v);
    static char big[1 << 20];
    int fl = fcntl(cl, F_GETFL);
    fcntl(cl, F_SETFL, fl & ~O_NONBLOCK);
    pid_t ch = fork();
    if (ch == 0) {
        _exit(send(cl, big, sizeof big, 0) == sizeof big ? 0 : 1);
    }
    long total = 0;
    ssize_t k;
    while (total < (long)sizeof big && (k = recv(ac, big, sizeof big, 0)) > 0) total += k;
    check("tcp-bulk", status_of(ch) == 0 && total == sizeof big, "%ld bytes", total);
    check("tcp-dontwait", recv(ac, b, 1, MSG_DONTWAIT) == -1 && errno == EAGAIN, "%s", errname(errno));
    shutdown(cl, SHUT_WR);
    check("tcp-eof", recv(ac, b, 1, 0) == 0, "data");
    close(ac);
    close(cl);
    close(l);
    cl = socket(AF_INET, SOCK_STREAM, 0);
    check("tcp-refused", connect(cl, (void *)&sa, sizeof sa) == -1 && errno == ECONNREFUSED, "%s", errname(errno));
    close(cl);
    int u1 = socket(AF_INET, SOCK_DGRAM, 0), u2 = socket(AF_INET, SOCK_DGRAM, 0);
    sa.sin_port = 0;
    sl = sizeof sa;
    bind(u1, (void *)&sa, sizeof sa);
    getsockname(u1, (void *)&sa, &sl);
    struct iovec m1 = {"x", 1}, m2 = {"yy", 2};
    struct mmsghdr mm[2] = {{.msg_hdr = {.msg_name = &sa, .msg_namelen = sizeof sa, .msg_iov = &m1, .msg_iovlen = 1}},
                            {.msg_hdr = {.msg_name = &sa, .msg_namelen = sizeof sa, .msg_iov = &m2, .msg_iovlen = 1}}};
    check("udp-sendmmsg", sendmmsg(u2, mm, 2, 0) == 2, "sent");
    check("udp-peek", recv(u1, b, sizeof b, MSG_PEEK) == 1 && recvfrom(u1, b, sizeof b, 0, NULL, NULL) == 1, "peek");
    struct mmsghdr rm[1] = {{.msg_hdr = {.msg_iov = (struct iovec[]){{b, sizeof b}}, .msg_iovlen = 1}}};
    check("udp-recvmmsg", recvmmsg(u1, rm, 1, 0, NULL) == 1 && rm[0].msg_len == 2, "len");
    close(u1);
    close(u2);
    int l6 = socket(AF_INET6, SOCK_STREAM, 0);
    struct sockaddr_in6 s6 = {.sin6_family = AF_INET6, .sin6_addr = IN6ADDR_ANY_INIT};
    int zero = 0;
    setsockopt(l6, IPPROTO_IPV6, IPV6_V6ONLY, &zero, sizeof zero);
    sl = sizeof s6;
    check("tcp6-dual-listen", bind(l6, (void *)&s6, sizeof s6) == 0 && listen(l6, 4) == 0 &&
                                  getsockname(l6, (void *)&s6, &sl) == 0, "listen");
    sa.sin_port = s6.sin6_port;
    cl = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in6 p6;
    sl = sizeof p6;
    check("tcp6-v4-mapped", connect(cl, (void *)&sa, sizeof sa) == 0 && (ac = accept(l6, (void *)&p6, &sl)) >= 0 &&
                                IN6_IS_ADDR_V4MAPPED(&p6.sin6_addr), "peer");
    close(ac);
    close(cl);
    s6.sin6_addr = in6addr_loopback;
    cl = socket(AF_INET6, SOCK_STREAM, 0);
    check("tcp6-loopback", connect(cl, (void *)&s6, sizeof s6) == 0 && (ac = accept(l6, NULL, NULL)) >= 0, "connect");
    close(ac);
    close(cl);
    close(l6);
    struct ifaddrs *ifa, *i;
    int lo4 = 0;
    if (getifaddrs(&ifa) == 0) {
        for (i = ifa; i; i = i->ifa_next)
            if (i->ifa_addr && i->ifa_addr->sa_family == AF_INET && !strcmp(i->ifa_name, "lo") &&
                ((struct sockaddr_in *)i->ifa_addr)->sin_addr.s_addr == htonl(INADDR_LOOPBACK))
                lo4 = 1;
        freeifaddrs(ifa);
    }
    check("getifaddrs-netlink", lo4, "no lo 127.0.0.1");
    check("if-nametoindex", if_nametoindex("lo") >= 1, "index");
}

// ---- procfs ----

static int file_has(const char *f, const char *needle) {
    static char buf[1 << 16];
    int fd = open(f, O_RDONLY);
    if (fd < 0) return 0;
    ssize_t n = read(fd, buf, sizeof buf - 1);
    close(fd);
    buf[n > 0 ? n : 0] = 0;
    return strstr(buf, needle) != NULL;
}

static void procfs(void) {
    group = "procfs";
    char b[4096];
    ssize_t n = readlink("/proc/self/exe", b, sizeof b - 1);
    b[n > 0 ? n : 0] = 0;
    check("self-exe", n > 0 && !strcmp(strrchr(b, '/') + 1, "lxprobe"), "%s", b);
    check("self-fd", access("/proc/self/fd/0", F_OK) == 0 && access("/proc/self/fd/2", F_OK) == 0, "fds");
    char want[64];
    snprintf(want, sizeof want, "Pid:\t%d\n", getpid());
    check("self-status", file_has("/proc/self/status", want), "no %s", "Pid line");
    check("self-maps", file_has("/proc/self/maps", "[stack]"), "no stack");
    check("meminfo", file_has("/proc/meminfo", "MemTotal:"), "no MemTotal");
    check("self-stat", file_has("/proc/self/stat", ") R "), "state");
    check("cmdline", file_has("/proc/self/cmdline", argv0), "argv0");
}

// ---- cred ----

static void cred(void) {
    group = "cred";
    uid_t r, e, s;
    getresuid(&r, &e, &s);
    check("resuid", r == getuid() && e == geteuid() && s == e, "ids");
    check("umask", umask(027) != 0777 && umask(022) == 027, "umask");
    if (getuid() != 0) {
        value("identity", "non-root");
        check("setuid-eperm", setuid(0) == -1 && errno == EPERM, "%s", errname(errno));
        return;
    }
    value("identity", "root");
    pid_t c = fork();
    if (c == 0) {
        gid_t g[1] = {65534};
        int ok = setgroups(1, g) == 0 && setresgid(65534, 65534, 65534) == 0 && setresuid(65534, 65534, 65534) == 0;
        getresuid(&r, &e, &s);
        ok = ok && r == 65534 && e == 65534 && s == 65534 && getgroups(1, g) == 1 && g[0] == 65534;
        _exit(ok && setuid(0) == -1 && errno == EPERM ? 0 : 1);
    }
    check("drop-to-nobody", status_of(c) == 0, "child");
}

static const struct { const char *name; void (*fn)(void); } groups[] = {
    {"proc", proc}, {"mem", mem}, {"sig", sig}, {"time", timeprobe}, {"file", file},
    {"pipe", pipes}, {"event", events}, {"sock", sock}, {"procfs", procfs}, {"cred", cred}};

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "exec-child")) {
        if (argc > 2 && !strcmp(argv[2], "check-closed"))
            return fcntl(atoi(argv[3]), F_GETFD) == -1 && errno == EBADF && fcntl(atoi(argv[4]), F_GETFD) == 0 ? 0 : 1;
        return argc;
    }
    argv0 = argv[0];
    ssize_t n = readlink("/proc/self/exe", self, sizeof self - 1);
    if (n <= 0) {
        printf("FAIL setup readlink /proc/self/exe (errno %s)\n", errname(errno));
        return 1;
    }
    snprintf(tmp, sizeof tmp, "/tmp/lxprobe.XXXXXX");
    if (!mkdtemp(tmp)) {
        printf("FAIL setup mkdtemp (errno %s)\n", errname(errno));
        return 1;
    }
    setvbuf(stdout, NULL, _IOLBF, 0);
    for (size_t i = 0; i < sizeof groups / sizeof groups[0]; i++) {
        int want = argc == 1;
        for (int a = 1; a < argc; a++) want |= !strcmp(argv[a], groups[i].name);
        if (want) groups[i].fn();
    }
    rmdir(tmp);
    printf("lxprobe: %d failure(s)\n", fails);
    return fails > 125 ? 125 : fails;
}
