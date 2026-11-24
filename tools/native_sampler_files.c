// One read-only descriptor snapshot. The parent owns the target and bounds this process.
#include <errno.h>
#include <libproc.h>
#include <mach/mach_time.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/proc_info.h>
#include <sys/resource.h>
#include <unistd.h>

static mach_timebase_info_data_t timebase;
static uint64_t now(void) {
  return (uint64_t)((__uint128_t)mach_absolute_time()*timebase.numer/timebase.denom);
}
static void string(const char *s) {
  putchar('"');
  for (const unsigned char *p=(const unsigned char *)s; *p; p++) {
    if (*p=='"' || *p=='\\') { putchar('\\'); putchar(*p); }
    else if (*p<32) printf("\\u%04x",*p);
    else putchar(*p);
  }
  putchar('"');
}
static bool identity(pid_t pid, uint64_t start, const char *uuid, bool live) {
  struct rusage_info_v4 info={0};
  if (proc_pid_rusage(pid,RUSAGE_INFO_V4,(rusage_info_t *)&info)) return false;
  char actual[33];
  for (unsigned i=0;i<16;i++) snprintf(actual+2*i,3,"%02x",info.ri_uuid[i]);
  return info.ri_proc_start_abstime==start && !strcmp(actual,uuid) && (!live || !info.ri_proc_exit_abstime);
}
int main(int argc,char **argv) {
  if (argc==2 && !strcmp(argv[1],"--ready")) {
    printf("{\"pid\":%d,\"descriptor_bytes\":%zu,\"vnode_path_bytes\":%zu}\n",getpid(),sizeof(struct proc_fdinfo),sizeof(struct vnode_fdinfowithpath));
    return 0;
  }
  if (argc!=5 || mach_timebase_info(&timebase)!=KERN_SUCCESS) return 64;
  pid_t pid=(pid_t)strtol(argv[1],NULL,10);
  uint64_t start=strtoull(argv[2],NULL,10),deadline=strtoull(argv[4],NULL,10);
  if (pid<=0 || !start || strlen(argv[3])!=32 || !deadline) return 64;
  setvbuf(stdout,NULL,_IONBF,0);
  printf("{\"phase\":\"start\",\"pid\":%d,\"target_pid\":%d,\"host_ns\":%llu}\n",getpid(),pid,now());
  bool verified=false,complete=false;const char *error="deadline";int saved_errno=0,count=0;
  if (now()>=deadline) goto finish;
  if (!identity(pid,start,argv[3],true)) { error="target identity unavailable or changed";goto finish; }
  struct proc_fdinfo entries[65];
  if (now()>=deadline) goto finish;
  int bytes=proc_pidinfo(pid,PROC_PIDLISTFDS,0,entries,sizeof(entries));
  if (bytes<=0 || (size_t)bytes>sizeof(entries) || bytes%sizeof(entries[0])) {
    saved_errno=errno;error="descriptor enumeration unavailable";goto finish;
  }
  count=bytes/sizeof(entries[0]);complete=count<=64;error=complete ? NULL : "descriptor limit";
  for (int i=0;i<count && i<64;i++) {
    if (now()>=deadline) { complete=false;error="deadline";goto finish; }
    if (entries[i].proc_fdtype!=PROX_FDTYPE_VNODE) continue;
    struct vnode_fdinfowithpath info={0};uint64_t began=now();errno=0;
    int n=proc_pidfdinfo(pid,entries[i].proc_fd,PROC_PIDFDVNODEPATHINFO,&info,sizeof(info));
    int code=errno;uint64_t ended=now();
    printf("{\"phase\":\"file\",\"fd\":%d,\"started_monotonic_ns\":%llu,\"finished_monotonic_ns\":%llu,",entries[i].proc_fd,began,ended);
    if (n!=(int)sizeof(info) || info.pfi.fi_type!=PROX_FDTYPE_VNODE || !info.pvip.vip_path[0]) {
      printf("\"status\":\"unavailable\",\"errno\":%d,\"bytes\":%d}\n",code,n);
      complete=false;error="descriptor closed, changed or unavailable";continue;
    }
    struct vinfo_stat *s=&info.pvip.vip_vi.vi_stat;
    printf("\"status\":\"available\",\"offset\":%lld,\"device\":%u,\"inode\":%llu,\"generation\":%u,\"mode\":%u,\"size\":%lld,\"path\":",
      info.pfi.fi_offset,s->vst_dev,s->vst_ino,s->vst_gen,s->vst_mode,s->vst_size);
    string(info.pvip.vip_path);puts("}");
  }
  if (now()>=deadline) { complete=false;error="deadline";goto finish; }
  verified=identity(pid,start,argv[3],false);
  if (!verified) { complete=false;error="target identity unavailable or changed"; }
  if (now()>=deadline) { complete=false;error="deadline"; }
finish:
  printf("{\"phase\":\"end\",\"identity_verified\":%s,\"complete\":%s,\"enumerated\":%d,\"errno\":%d,\"host_ns\":%llu,\"error\":",
    verified?"true":"false",complete?"true":"false",count,saved_errno,now());
  if (error) string(error);else printf("null");puts("}");
  return complete && verified ? 0 : 1;
}
