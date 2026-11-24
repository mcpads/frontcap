// Experimental ABI checks; no virtual-wait interception is enabled.
#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <linux/futex.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>
int syscall_forward_control(void) {
  errno=ERANGE;
  assert(syscall(SYS_getpid)==getpid());assert(errno==ERANGE);
  assert(syscall(SYS_close,-1)==-1 && errno==EBADF);
  int pipefd[2];assert(pipe(pipefd)==0);char sent='Q',received=0;
  assert(syscall(SYS_write,pipefd[1],&sent,1)==1);
  assert(syscall(SYS_read,pipefd[0],&received,1)==1 && received==sent);
  assert(syscall(SYS_close,pipefd[0])==0);assert(syscall(SYS_close,pipefd[1])==0);
  long memory=syscall(SYS_mmap,NULL,4096,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
  assert(memory!=-1);*(volatile uint64_t *)memory=UINT64_C(0xfedcba9876543210);
  assert(*(volatile uint64_t *)memory==UINT64_C(0xfedcba9876543210));
  assert(syscall(SYS_munmap,(void *)memory,4096)==0);
  uint32_t word=1;
  assert(syscall(SYS_futex,&word,FUTEX_WAIT_BITSET_PRIVATE,0,NULL,NULL,0)==-1 && errno==EINVAL);
  assert(syscall(SYS_futex,&word,FUTEX_WAIT_BITSET_PRIVATE,0,NULL,NULL,1)==-1 && errno==EAGAIN);
  puts("{\"case\":\"syscall-forwarding\",\"passed\":true}");return 0;
}
#ifdef SYSCALL_FORWARD_STANDALONE
int main(void) {return syscall_forward_control();}
#endif
