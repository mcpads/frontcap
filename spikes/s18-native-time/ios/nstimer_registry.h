// Internal to dispatch_activity.m. Only copied timer metadata is read under activity_lock.
#import <objc/runtime.h>

@interface FCTimerModeQuery : NSObject
@property(nonatomic) CFRunLoopRef loop;
@property(nonatomic, strong) NSString *mode, *key;
@end
@implementation FCTimerModeQuery
- (void)dealloc { if (_loop) CFRelease(_loop); }
@end
@interface FCTimerActivity : NSObject
@property(nonatomic) BOOL observed, engaged, valid;
@property(nonatomic) CFAbsoluteTime date;
@property(nonatomic) uint64_t revision, mutations, callbacks, task_id;
@property(nonatomic, strong) NSMutableSet<NSString *> *modes;
@property(nonatomic, strong) NSMutableDictionary<NSString *, NSString *> *mode_names;
@property(nonatomic, strong) NSMutableDictionary<NSString *, FCTimerModeQuery *> *queries;
@end
@implementation FCTimerActivity
@end
static NSMutableSet<FCTimerActivity *> *registered_timers;
static void reconcile_timer_locked(FCTimerActivity *state);
@interface FCTimerLifetime : NSObject
@property(nonatomic, strong) FCTimerActivity *state;
@end
@implementation FCTimerLifetime
- (void)dealloc {
  lock_activity();
  _state.engaged = NO;
  reconcile_timer_locked(_state);
  unlock_activity();
}
@end
static char timer_lifetime_key;
static FCTimerActivity *timer_state_locked(NSTimer *timer) {
  FCTimerLifetime *lifetime = objc_getAssociatedObject(timer, &timer_lifetime_key);
  if (!lifetime) {
    lifetime = [FCTimerLifetime new];
    lifetime.state = [FCTimerActivity new];
    lifetime.state.modes = [NSMutableSet new];
    lifetime.state.mode_names = [NSMutableDictionary new];
    lifetime.state.queries = [NSMutableDictionary new];
    objc_setAssociatedObject(timer, &timer_lifetime_key, lifetime, OBJC_ASSOCIATION_RETAIN_NONATOMIC);
  }
  return lifetime.state;
}
static void revise_timer_locked(FCTimerActivity *state) {
  if (++state.revision == 0) abort();
}
static void reconcile_timer_locked(FCTimerActivity *state) {
  // Preserve live schedules before tracking without admitting due work yet.
  BOOL scheduled = state.engaged && state.valid && state.modes.count;
  if (scheduled) {
    if (!registered_timers) registered_timers = [NSMutableSet new];
    [registered_timers addObject:state];
  } else {
    [registered_timers removeObject:state];
  }
  // A running invocation owns its separate task until native return. Mutations
  // cannot cancel that invocation, even if they invalidate its future schedule.
  BOOL due = tracking && scheduled && !state.callbacks && state.date <= CFAbsoluteTimeGetCurrent();
  if (due && !state.task_id) state.task_id = admit_task(NULL);
  if (!due && state.task_id) {
    finish_task_locked(state.task_id);
    state.task_id = 0;
  }
}
// Observe destruction without retaining the native loop. A stopped/exited thread
// can leave a retained loop and its timers alive, so thread exit is not retirement.
@interface FCTimerLoopLifetime : NSObject
@property(nonatomic, copy) NSString *prefix;
@end
@implementation FCTimerLoopLifetime
- (void)dealloc {
  lock_activity();
  for (FCTimerActivity *state in [registered_timers allObjects]) {
    for (NSString *key in [state.modes allObjects]) {
      if (![key hasPrefix:_prefix]) continue;
      [state.modes removeObject:key];
      [state.mode_names removeObjectForKey:key];
    }
    reconcile_timer_locked(state);
  }
  unlock_activity();
}
@end
static char timer_loop_lifetime_key;
static FCTimerActivity *timer_mutation_begin(NSTimer *timer, CFRunLoopRef loop,
                                            CFStringRef mode, BOOL engage) {
  NSString *mode_name = mode ? [NSString stringWithString:(__bridge NSString *)mode] : nil;
  NSString *prefix = loop ? [NSString stringWithFormat:@"%p:", loop] : nil;
  lock_activity();
  if (loop && !objc_getAssociatedObject((__bridge id)loop, &timer_loop_lifetime_key)) {
    FCTimerLoopLifetime *lifetime = [FCTimerLoopLifetime new];
    lifetime.prefix = prefix;
    objc_setAssociatedObject((__bridge id)loop, &timer_loop_lifetime_key, lifetime,
                             OBJC_ASSOCIATION_RETAIN_NONATOMIC);
  }
  FCTimerActivity *state = timer_state_locked(timer);
  revise_timer_locked(state);
  state.mutations++;
  if (engage && state.observed) state.engaged = YES;
  if (mode_name) state.mode_names[[prefix stringByAppendingString:mode_name]] = mode_name;
  NSDictionary<NSString *, NSString *> *names = loop ? [state.mode_names copy] : nil;
  unlock_activity();
  // Common-mode removal can also remove a separately remembered concrete mode.
  // Requery every known membership on this loop, not only the named mode.
  for (NSString *key in names) {
    if (![key hasPrefix:prefix]) continue;
    FCTimerModeQuery *query = [FCTimerModeQuery new];
    query.loop = (CFRunLoopRef)CFRetain(loop);
    query.mode = names[key]; query.key = key;
    lock_activity();
    state.queries[key] = query;
    unlock_activity();
  }
  return state;
}
id fc_nstimer_mutation_begin(NSTimer *timer, CFRunLoopRef loop, CFStringRef mode) {
  return timer ? timer_mutation_begin(timer, loop, mode, YES) : nil;
}
void fc_nstimer_mutation_end(FCTimerActivity *state, NSTimer *timer) {
  if (!state) return;
  lock_activity();
  if (!state.mutations) abort();
  state.mutations--;
  if (state.mutations) { unlock_activity(); return; }
  uint64_t revision = state.revision;
  BOOL observed = state.observed;
  __attribute__((objc_precise_lifetime)) NSArray<FCTimerModeQuery *> *queries = [state.queries.allValues copy];
  unlock_activity();

  // Native getters can take native locks or invoke Objective-C code. The caller
  // retains an activity obligation throughout this read/commit sequence.
  CFRunLoopTimerRef native = (__bridge CFRunLoopTimerRef)timer;
  // Unwrapped CF timers only stage membership for a possible enclosing NSTimer
  // factory. Do not inspect their deadlines or change their callback coverage.
  BOOL valid = !observed || CFRunLoopTimerIsValid(native);
  CFAbsoluteTime date = observed && valid ? CFRunLoopTimerGetNextFireDate(native) : 0;
  NSMutableArray<NSNumber *> *membership = [NSMutableArray new];
  for (FCTimerModeQuery *query in queries)
    [membership addObject:@(valid && CFRunLoopContainsTimer(query.loop, native, (__bridge CFStringRef)query.mode))];

  lock_activity();
  if (state.revision == revision && !state.mutations) {
    for (NSUInteger i = 0; i < queries.count; i++) {
      if (membership[i].boolValue) [state.modes addObject:queries[i].key];
      else {
        [state.modes removeObject:queries[i].key];
        [state.mode_names removeObjectForKey:queries[i].key];
      }
    }
    [state.queries removeAllObjects]; // queries retains native inputs until after unlock.
    state.valid = valid;
    state.date = date;
    if (!valid) { [state.modes removeAllObjects]; [state.mode_names removeAllObjects]; }
    reconcile_timer_locked(state);
  }
  // A concurrent mutation/callback invalidates this snapshot. Its final reader
  // owns publication; an older read must never overwrite newer native state.
  unlock_activity();
}
void fc_nstimer_created(NSTimer *timer) {
  if (!timer) return;
  FCTimerActivity *state = timer_mutation_begin(timer, NULL, NULL, NO);
  lock_activity();
  state.observed = YES;
  state.engaged = YES;
  unlock_activity();
  fc_nstimer_mutation_end(state, timer);
}
static void admit_due_nstimers(void) {
  // All entries here remain scheduled; admission cannot mutate this set.
  for (FCTimerActivity *state in registered_timers) {
    if (!state.callbacks && !state.task_id && state.date <= CFAbsoluteTimeGetCurrent())
      state.task_id = admit_task(NULL);
  }
}
static void refresh_returned_timer(FCTimerActivity *state, NSTimer *timer) {
  FCTimerActivity *current = timer_mutation_begin(timer, NULL, NULL, NO);
  if (current != state) abort();
  fc_nstimer_mutation_end(state, timer);
}
static void finish_timer_callback(FCTimerActivity *state, struct callback_scope *scope) {
  lock_activity();
  if (!state.callbacks) abort();
  state.callbacks--;
  reconcile_timer_locked(state);
  finish_callback_locked(scope);
  unlock_activity();
  free(scope);
}
