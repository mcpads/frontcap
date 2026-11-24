/* Opt-in diagnostic: stop the adbd child before exec of the real input command. */
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static long long now(void) {
    struct timespec value;
    if (clock_gettime(CLOCK_MONOTONIC, &value)) exit(71);
    return (long long)value.tv_sec*1000000000LL+value.tv_nsec;
}
static int identity(const char *value) {
    if (strlen(value)!=36) return 0;
    for (int i=0;i<36;i++) {
        if (i==8 || i==13 || i==18 || i==23) { if (value[i]!='-') return 0; }
        else if (!strchr("0123456789abcdef",value[i])) return 0;
    }
    return 1;
}
static long long positive(const char *value, long long limit) {
    if (!*value || strspn(value,"0123456789")!=strlen(value)) exit(64);
    errno=0;char *end;long long result=strtoll(value,&end,10);
    if (errno || *end || result<=0 || result>limit) exit(64);
    return result;
}
int main(int argc,char **argv) {
    if (argc!=7 || !identity(argv[2]) || !identity(argv[3])) return 64;
    long long request=positive(argv[4],9223372036854775807LL);
    long long x=positive(argv[5],2147483647),y=positive(argv[6],2147483647);
    if (argv[1][0]!='/' || chdir(argv[1])) return 64;
    struct sigaction hup;sigset_t blocked;
    if (sigaction(SIGHUP,NULL,&hup) || sigprocmask(SIG_SETMASK,NULL,&blocked)) return 65;
    FILE *out=fopen("ready.tmp","wx");if (!out) return 66;
    fprintf(out,"{\"pid\":%d,\"ppid\":%d,\"group\":%d,\"uid\":%d,\"monotonic_ns\":%lld,\"hup_default\":%s,\"hup_blocked\":%s,\"request\":{\"generation\":\"%s\",\"controller_id\":\"%s\",\"request_id\":%lld,\"operation\":\"tap\",\"target\":\"native-increment\",\"x\":%lld,\"y\":%lld}}\n",
        getpid(),getppid(),getpgrp(),getuid(),now(),hup.sa_handler==SIG_DFL?"true":"false",sigismember(&blocked,SIGHUP)?"true":"false",argv[2],argv[3],request,x,y);
    if (fclose(out) || rename("ready.tmp","ready.json")) return 67;
    printf("GATE_READY %d\n",getpid());fflush(stdout);
    if (raise(SIGSTOP)) return 68;
    out=fopen("released.json","wx");if (!out) return 69;
    fprintf(out,"{\"pid\":%d,\"monotonic_ns\":%lld}\n",getpid(),now());
    if (fclose(out)) return 70;
    execl("/system/bin/input","input","tap",argv[5],argv[6],(char *)NULL);
    perror("exec input");return 72;
}
