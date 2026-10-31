/* Minimal authoritative UDP DNS responder for scripts/perf_ab.py.
 *
 * The userspace benchmark arm forwards every query, so a slow upstream would
 * become the measurement. This answers every A query with a fixed address and
 * TTL in a tight recvfrom/sendto loop, and appends its running query count to
 * the file given by --count-file when it receives SIGUSR1. The runner uses
 * that delta to prove the XDP arm answered in the kernel instead of passing
 * queries through to userspace.
 *
 * Build: cc -O2 -static -o perf-upstream scripts/perf_upstream.c
 */
#define _GNU_SOURCE

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static volatile unsigned long long received;
static int count_fd = -1;

static void dump_count(int sig) {
    (void)sig;
    char digits[32];
    char line[40];
    unsigned long long value = received;
    int n = 0;

    do {
        digits[n++] = (char)('0' + value % 10);
        value /= 10;
    } while (value != 0);

    for (int i = 0; i < n; i++) {
        line[i] = digits[n - 1 - i];
    }
    line[n++] = '\n';

    /* write(2) is async-signal-safe; the counter itself is approximation-only. */
    ssize_t ignored = write(count_fd, line, (size_t)n);
    (void)ignored;
}

static int question_end(const unsigned char *query, size_t len, size_t *end,
                        unsigned *qtype) {
    if (len < 12 || ((query[4] << 8) | query[5]) != 1) {
        return 0;
    }

    size_t i = 12;
    while (i < len) {
        unsigned label = query[i];
        if ((label & 0xC0) != 0) {
            return 0;
        }
        if (label == 0) {
            i++;
            break;
        }
        i += 1 + label;
    }
    if (i + 4 > len) {
        return 0;
    }

    *qtype = ((unsigned)query[i] << 8) | query[i + 1];
    *end = i + 4;
    return 1;
}

int main(int argc, char **argv) {
    unsigned port = 15354;
    const char *ip_text = "127.0.0.9";
    unsigned ttl = 300;
    const char *count_file = NULL;

    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--port") && i + 1 < argc) {
            port = (unsigned)strtoul(argv[++i], NULL, 10);
        } else if (!strcmp(argv[i], "--ip") && i + 1 < argc) {
            ip_text = argv[++i];
        } else if (!strcmp(argv[i], "--ttl") && i + 1 < argc) {
            ttl = (unsigned)strtoul(argv[++i], NULL, 10);
        } else if (!strcmp(argv[i], "--count-file") && i + 1 < argc) {
            count_file = argv[++i];
        } else {
            fprintf(stderr,
                    "usage: %s [--port 15354] [--ip 127.0.0.9] [--ttl 300] "
                    "[--count-file PATH]\n",
                    argv[0]);
            return 2;
        }
    }

    if (port == 0 || port > 65535 || ttl > 0x7fffffffU) {
        fprintf(stderr, "invalid port or ttl\n");
        return 2;
    }

    unsigned char answer_ip[4];
    if (inet_pton(AF_INET, ip_text, answer_ip) != 1) {
        fprintf(stderr, "invalid --ip: %s\n", ip_text);
        return 2;
    }

    if (count_file != NULL) {
        count_fd = open(count_file, O_WRONLY | O_CREAT | O_TRUNC | O_APPEND, 0644);
        if (count_fd < 0) {
            perror("open count file");
            return 1;
        }
        if (write(count_fd, "0\n", 2) != 2) {
            perror("write count file");
            return 1;
        }
        struct sigaction action;
        memset(&action, 0, sizeof action);
        action.sa_handler = dump_count;
        sigemptyset(&action.sa_mask);
        action.sa_flags = SA_RESTART;
        if (sigaction(SIGUSR1, &action, NULL) != 0) {
            perror("sigaction");
            return 1;
        }
    }

    int socket_fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (socket_fd < 0) {
        perror("socket");
        return 1;
    }

    int buffer_size = 4 * 1024 * 1024;
    setsockopt(socket_fd, SOL_SOCKET, SO_RCVBUF, &buffer_size, sizeof buffer_size);
    setsockopt(socket_fd, SOL_SOCKET, SO_SNDBUF, &buffer_size, sizeof buffer_size);

    struct sockaddr_in address;
    memset(&address, 0, sizeof address);
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    address.sin_port = htons((uint16_t)port);
    if (bind(socket_fd, (struct sockaddr *)&address, sizeof address) != 0) {
        perror("bind");
        return 1;
    }

    fprintf(stdout, "perf-upstream listening on 127.0.0.1:%u ip=%s ttl=%u\n",
            port, ip_text, ttl);
    fflush(stdout);

    unsigned char query[4096];
    /* The question can occupy the whole query, and the A record appended to
     * it is 16 bytes: pointer(2) + type(2) + class(2) + ttl(4) + rdlen(2) +
     * address(4). */
    unsigned char response[sizeof query + 16];
    for (;;) {
        struct sockaddr_storage peer;
        socklen_t peer_len = sizeof peer;
        ssize_t len = recvfrom(socket_fd, query, sizeof query, 0,
                               (struct sockaddr *)&peer, &peer_len);
        if (len < 0) {
            if (errno == EINTR) {
                continue;
            }
            perror("recvfrom");
            return 1;
        }

        size_t qend = 0;
        unsigned qtype = 0;
        if (!question_end(query, (size_t)len, &qend, &qtype)) {
            continue;
        }

        memcpy(response, query, qend);
        response[2] = 0x81; /* QR=1, RD=1 */
        response[3] = 0x80; /* RA=1, RCODE=NOERROR */
        response[4] = 0; /* QDCOUNT 1, already in the copied question */
        response[5] = 1;
        response[6] = 0;
        response[7] = (qtype == 1) ? 1 : 0; /* ANCOUNT */
        response[8] = response[9] = 0;      /* NSCOUNT */
        response[10] = response[11] = 0;    /* ARCOUNT */

        size_t off = qend;
        if (qtype == 1) {
            response[off++] = 0xC0; /* name pointer to the question */
            response[off++] = 0x0C;
            response[off++] = 0x00; /* type A */
            response[off++] = 0x01;
            response[off++] = 0x00; /* class IN */
            response[off++] = 0x01;
            response[off++] = (unsigned char)(ttl >> 24);
            response[off++] = (unsigned char)(ttl >> 16);
            response[off++] = (unsigned char)(ttl >> 8);
            response[off++] = (unsigned char)ttl;
            response[off++] = 0x00; /* RDLENGTH 4 */
            response[off++] = 0x04;
            memcpy(response + off, answer_ip, 4);
            off += 4;
        }

        if (sendto(socket_fd, response, off, 0, (struct sockaddr *)&peer,
                   peer_len) < 0) {
            perror("sendto");
        }
        received++;
    }
}
