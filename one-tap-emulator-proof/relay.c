// Throwaway: forwards a local port to the device's direct port from source 100.64.0.2, so RigDesk's tailnet gate sees a tailnet peer.
#include <arpa/inet.h>
#include <netinet/in.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <unistd.h>
static void die(const char *w) { perror(w); exit(1); }
int main(int argc, char **argv) {
    if (argc != 3) { fprintf(stderr, "relay <listen-port> <target-port>\n"); return 2; }
    signal(SIGCHLD, SIG_IGN); signal(SIGPIPE, SIG_IGN);
    int l = socket(AF_INET, SOCK_STREAM, 0), on = 1;
    setsockopt(l, SOL_SOCKET, SO_REUSEADDR, &on, sizeof on);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(atoi(argv[1])), .sin_addr.s_addr = htonl(INADDR_ANY)};
    if (bind(l, (void *)&a, sizeof a) || listen(l, 16)) die("listen");
    for (;;) {
        int c = accept(l, 0, 0);
        if (c < 0) continue;
        if (fork()) { close(c); continue; }
        int s = socket(AF_INET, SOCK_STREAM, 0);
        struct sockaddr_in src = {.sin_family = AF_INET}, dst = {.sin_family = AF_INET, .sin_port = htons(atoi(argv[2]))};
        inet_pton(AF_INET, "100.64.0.2", &src.sin_addr); inet_pton(AF_INET, "100.64.0.2", &dst.sin_addr);
        if (bind(s, (void *)&src, sizeof src)) die("bind source");
        if (connect(s, (void *)&dst, sizeof dst)) die("connect");
        struct pollfd p[2] = {{c, POLLIN, 0}, {s, POLLIN, 0}};
        char b[65536];
        while (poll(p, 2, -1) > 0)
            for (int i = 0; i < 2; i++)
                if (p[i].revents) {
                    ssize_t n = read(p[i].fd, b, sizeof b);
                    if (n <= 0) _exit(0);
                    for (ssize_t o = 0; o < n;) { ssize_t w = write(p[!i].fd, b + o, n - o); if (w <= 0) _exit(0); o += w; }
                }
        _exit(0);
    }
}
