/* Run a command; print wall time (ms) and the child's peak RSS (KiB) to stderr.
   A C wrapper so the fork does not inherit a large parent (e.g. Python). */
#include <stdio.h>
#include <stdlib.h>
#include <sys/resource.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
int main(int argc, char **argv) {
  struct timespec a, b; struct rusage ru; int st;
  clock_gettime(CLOCK_MONOTONIC, &a);
  pid_t p = fork();
  if (p == 0) { execvp(argv[1], argv + 1); _exit(127); }
  wait4(p, &st, 0, &ru);
  clock_gettime(CLOCK_MONOTONIC, &b);
  fprintf(stderr, "MEASURE %.1f %ld\n", (b.tv_sec - a.tv_sec) * 1e3 + (b.tv_nsec - a.tv_nsec) / 1e6, ru.ru_maxrss);
  return WIFEXITED(st) ? WEXITSTATUS(st) : 1;
}
