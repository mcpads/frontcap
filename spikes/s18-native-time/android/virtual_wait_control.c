// Standalone behavioral control. A host alarm bounds failed/no-wake experiments.
#define _GNU_SOURCE
#include <assert.h>
#include <signal.h>
#include <stdio.h>
#include <stdatomic.h>
#include <unistd.h>
static _Atomic int hold_before_sleep,registered,release_sleep;
static void before_sleep(void) {
  atomic_fetch_add(&registered,1);
  while(atomic_load(&hold_before_sleep) && !atomic_load(&release_sleep)) usleep(1000);
}
#define VIRTUAL_WAIT_BEFORE_SLEEP() before_sleep()
#include "virtual_wait.h"
static struct virtual_wait_clock clock_state=VIRTUAL_WAIT_CLOCK_INIT;
static _Atomic uint32_t first,second;
static _Atomic int done;
static int outcome;
static void *worker(void *unused) {
  (void)unused;outcome=virtual_wait(&clock_state,&first,0,5000);
  atomic_store(&done,1);return NULL;
}
static pthread_t start(int hold) {
  atomic_store(&registered,0);atomic_store(&release_sleep,0);atomic_store(&done,0);
  atomic_store(&hold_before_sleep,hold);pthread_t t;
  assert(pthread_create(&t,NULL,worker,NULL)==0);
  while(!atomic_load(&registered)) usleep(1000);
  return t;
}
static void reset(void) {
  assert(!clock_state.waiters);clock_state.now=0;
  atomic_store(&first,0);atomic_store(&second,0);
}
static void finish(pthread_t t,int expected,const char *name) {
  assert(pthread_join(t,NULL)==0);assert(outcome==expected);assert(!clock_state.waiters);
  printf("{\"case\":\"%s\",\"outcome\":%d}\n",name,outcome);
}
struct wait_result { int outcome; _Atomic int done; };
static void *independent_worker(void *argument) {
  struct wait_result *result=argument;
  result->outcome=virtual_wait(&clock_state,&first,0,5000);
  atomic_store(&result->done,1);return NULL;
}
static void multiple_waiters(void) {
  reset();atomic_store(&registered,0);atomic_store(&hold_before_sleep,1);
  atomic_store(&release_sleep,0);
  pthread_t threads[3];struct wait_result results[3]={0};
  for(int i=0;i<3;i++) assert(pthread_create(&threads[i],NULL,independent_worker,&results[i])==0);
  while(atomic_load(&registered)!=3) usleep(1000);
  assert(virtual_wait_requeue(&clock_state,&first,0,&second,1,1)==2);
  assert(virtual_wait_requeue(&clock_state,&first,0,&second,10,0)==1);
  assert(virtual_wait_requeue(&clock_state,&second,0,&first,10,0)==1);
  assert(virtual_wait_requeue(&clock_state,&first,0,&second,10,0)==0);
  atomic_store(&release_sleep,1);
  for(int i=0;i<3;i++) {assert(pthread_join(threads[i],NULL)==0);assert(results[i].outcome==0);}
  assert(!clock_state.waiters);
  puts("{\"case\":\"bounded-multiple-wake-and-requeue\",\"passed\":true}");
}
static volatile sig_atomic_t signal_seen;
static void interrupt_wait(int number) {(void)number;signal_seen=1;}
static void interrupted_wait(void) {
  reset();atomic_store(&registered,0);atomic_store(&hold_before_sleep,0);
  signal_seen=0;
  struct sigaction action={.sa_handler=interrupt_wait},previous;
  sigemptyset(&action.sa_mask);assert(sigaction(SIGUSR2,&action,&previous)==0);
  struct wait_result result={0};pthread_t thread;
  assert(pthread_create(&thread,NULL,independent_worker,&result)==0);
  while(!atomic_load(&registered)) usleep(1000);
  // Repeat a targeted signal if the first arrives before kernel registration.
  // The process alarm bounds failure; no signal is sent after the thread is joined.
  while(!atomic_load(&result.done)) {
    int error=pthread_kill(thread,SIGUSR2);assert(error==0 || error==ESRCH);usleep(1000);
  }
  assert(pthread_join(thread,NULL)==0);assert(signal_seen && result.outcome==EINTR);
  assert(!clock_state.waiters);assert(sigaction(SIGUSR2,&previous,NULL)==0);
  assert(virtual_wait_requeue(&clock_state,&first,0,&second,10,0)==0);
  puts("{\"case\":\"signal-interruption-removes-waiter\",\"passed\":true}");
}
static _Atomic int race_start;
static int race_wakes;
static void *race_notify(void *argument) {
  (void)argument;while(!atomic_load(&race_start)) usleep(100);
  race_wakes=virtual_wait_requeue(&clock_state,&first,0,&second,1,0);return NULL;
}
static void *race_advance(void *argument) {
  (void)argument;while(!atomic_load(&race_start)) usleep(100);
  assert(virtual_wait_advance(&clock_state,5000)==0);return NULL;
}
static void concurrent_completion(void) {
  for(int i=0;i<20;i++) {
    reset();atomic_store(&race_start,0);pthread_t waiter=start(0),notify,advance;
    assert(pthread_create(&notify,NULL,race_notify,NULL)==0);
    assert(pthread_create(&advance,NULL,race_advance,NULL)==0);
    atomic_store(&race_start,1);
    assert(pthread_join(notify,NULL)==0);assert(pthread_join(advance,NULL)==0);
    assert(pthread_join(waiter,NULL)==0);
    assert((outcome==0 && race_wakes==1) || (outcome==ETIMEDOUT && race_wakes==0));
    assert(!clock_state.waiters);
  }
  puts("{\"case\":\"notification-deadline-race\",\"comparisons\":20,\"passed\":true}");
}
int main(void) {
  setbuf(stdout,NULL);alarm(5);printf("{\"pid\":%d}\n",getpid());
  reset();pthread_t t=start(0);
  usleep(20000);assert(!atomic_load(&done));
  assert(virtual_wait_advance(&clock_state,4999)==0);
  usleep(20000);assert(!atomic_load(&done));
  assert(virtual_wait_advance(&clock_state,5000)==0);
  finish(t,ETIMEDOUT,"frozen-before-exact-deadline");
  reset();t=start(1);assert(virtual_wait_advance(&clock_state,5000)==0);
  atomic_store(&release_sleep,1);finish(t,ETIMEDOUT,"advance-before-kernel-registration");
  reset();t=start(1);
  assert(virtual_wait_requeue(&clock_state,&first,0,&second,1,0)==1);
  atomic_store(&release_sleep,1);finish(t,0,"notification-before-kernel-registration");
  reset();t=start(0);
  assert(virtual_wait_requeue(&clock_state,&first,1,&second,0,1)==-EAGAIN);
  assert(virtual_wait_requeue(&clock_state,&first,0,&second,0,1)==1);
  assert(virtual_wait_requeue(&clock_state,&first,0,&second,1,0)==0);
  assert(virtual_wait_requeue(&clock_state,&second,0,&first,1,0)==1);
  finish(t,0,"requeue-then-destination-notification");
  reset();atomic_store(&first,1);
  assert(virtual_wait(&clock_state,&first,0,5000)==EAGAIN);
  atomic_store(&first,0);assert(virtual_wait_advance(&clock_state,5000)==0);
  assert(virtual_wait(&clock_state,&first,0,5000)==ETIMEDOUT);
  assert(virtual_wait_advance(&clock_state,4999)==EINVAL);
  puts("{\"case\":\"pre-registration-value-and-deadline\",\"passed\":true}");
  reset();t=start(1);atomic_store(&first,1);
  assert(virtual_wait_requeue_unchecked(&clock_state,&first,&second,0,1)==1);
  assert(virtual_wait_requeue(&clock_state,&second,0,&first,1,0)==1);
  atomic_store(&release_sleep,1);finish(t,0,"unconditional-requeue-after-sequence-change");
  multiple_waiters();interrupted_wait();concurrent_completion();
  alarm(0);return 0;
}
