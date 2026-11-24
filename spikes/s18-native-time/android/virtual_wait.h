// Experimental owned-wait registry, not an ART/syscall interceptor.
// All notifications/requeues for enrolled keys must pass through this registry.
#include <errno.h>
#include <limits.h>
#include <linux/futex.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef VIRTUAL_WAIT_BEFORE_SLEEP
#define VIRTUAL_WAIT_BEFORE_SLEEP() ((void)0)
#endif
struct virtual_waiter {
  _Atomic uint32_t ready;
  _Atomic uint32_t *key;
  uint64_t deadline;
  int outcome;
  struct virtual_waiter *next;
};
struct virtual_wait_clock {
  pthread_mutex_t lock;
  uint64_t now;
  struct virtual_waiter *waiters;
};
#define VIRTUAL_WAIT_CLOCK_INIT {PTHREAD_MUTEX_INITIALIZER, 0, NULL}

static void virtual_wait_complete(struct virtual_waiter *waiter, int outcome) {
  waiter->outcome=outcome;
  atomic_store(&waiter->ready,1);
  syscall(SYS_futex,&waiter->ready,FUTEX_WAKE_PRIVATE,1,NULL,NULL,0);
}
static int virtual_wait(struct virtual_wait_clock *clock, _Atomic uint32_t *key,
                        uint32_t expected, uint64_t deadline) {
  struct virtual_waiter waiter={.key=key,.deadline=deadline};
  pthread_mutex_lock(&clock->lock);
  if (atomic_load(key)!=expected) {pthread_mutex_unlock(&clock->lock);return EAGAIN;}
  if (clock->now>=deadline) {pthread_mutex_unlock(&clock->lock);return ETIMEDOUT;}
  waiter.next=clock->waiters;clock->waiters=&waiter;
  pthread_mutex_unlock(&clock->lock);
  VIRTUAL_WAIT_BEFORE_SLEEP();
  int error=0;
  while (!atomic_load(&waiter.ready)) {
    long result=syscall(SYS_futex,&waiter.ready,FUTEX_WAIT_PRIVATE,0,NULL,NULL,0);
    if (result<0 && errno!=EAGAIN) {error=errno;break;}
  }
  pthread_mutex_lock(&clock->lock);
  struct virtual_waiter **link=&clock->waiters;
  while (*link!=&waiter) link=&(*link)->next;
  *link=waiter.next;
  int outcome=atomic_load(&waiter.ready) ? waiter.outcome : error;
  pthread_mutex_unlock(&clock->lock);
  return outcome;
}
static int virtual_wait_advance(struct virtual_wait_clock *clock,uint64_t now) {
  pthread_mutex_lock(&clock->lock);
  if(now<clock->now) {pthread_mutex_unlock(&clock->lock);return EINVAL;}
  clock->now=now;
  for(struct virtual_waiter *w=clock->waiters;w;w=w->next)
    if(!atomic_load(&w->ready) && now>=w->deadline) virtual_wait_complete(w,ETIMEDOUT);
  pthread_mutex_unlock(&clock->lock);return 0;
}
// Owned waits share transfer logic; comparison is optional for FUTEX_REQUEUE.
static int virtual_wait_transfer(struct virtual_wait_clock *clock,_Atomic uint32_t *key,
                                 const uint32_t *expected,_Atomic uint32_t *destination,
                                 unsigned wake_count,unsigned move_count) {
  pthread_mutex_lock(&clock->lock);
  if(expected && atomic_load(key)!=*expected) {pthread_mutex_unlock(&clock->lock);return -EAGAIN;}
  int affected=0;
  for(struct virtual_waiter *w=clock->waiters;w;w=w->next) {
    if(w->key!=key || atomic_load(&w->ready)) continue;
    if(wake_count) {virtual_wait_complete(w,0);wake_count--;affected++;}
    else if(move_count) {w->key=destination;move_count--;affected++;}
  }
  pthread_mutex_unlock(&clock->lock);return affected;
}
static int virtual_wait_requeue(struct virtual_wait_clock *clock,_Atomic uint32_t *key,
                                uint32_t expected,_Atomic uint32_t *destination,
                                unsigned wake_count,unsigned move_count) {
  return virtual_wait_transfer(clock,key,&expected,destination,wake_count,move_count);
}
static int virtual_wait_requeue_unchecked(struct virtual_wait_clock *clock,_Atomic uint32_t *key,
                                          _Atomic uint32_t *destination,
                                          unsigned wake_count,unsigned move_count) {
  return virtual_wait_transfer(clock,key,NULL,destination,wake_count,move_count);
}
