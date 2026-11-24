// A real filesystem producer for the diagnostic; no timing-dependent observer race.
#include <stdio.h>
#include <string.h>
#include <unistd.h>
int main(int argc, char **argv) {
  if (argc != 3) return 2;
  FILE *file = fopen(argv[1], "wx");
  const char *payload = "{\"result\":\"prepared\"}\n";
  if (!file || fwrite(payload, 1, strlen(payload), file) != strlen(payload) ||
      fflush(file) != 0 || fsync(fileno(file)) != 0 || fclose(file) != 0) return 3;
  return rename(argv[1], argv[2]) == 0 ? 0 : 4;
}
