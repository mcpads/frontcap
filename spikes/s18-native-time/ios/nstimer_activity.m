// Experimental coverage of NSTimer's four interval factory methods, not arbitrary initializers.
#import <Foundation/Foundation.h>
#import <objc/runtime.h>
#import <objc/message.h>
#include <stdlib.h>

extern void fc_observe_callback(void (^block)(void));
extern void fc_observe_runloop_callback(NSTimer *timer, void (^block)(void));
extern void fc_nstimer_created(NSTimer *timer);
extern id fc_nstimer_mutation_begin(NSTimer *timer, CFRunLoopRef loop, CFStringRef mode);
extern void fc_nstimer_mutation_end(id state, NSTimer *timer);
static void mutate_timer(NSTimer *timer, CFRunLoopRef loop, CFStringRef mode, void (^body)(void)) {
  fc_observe_callback(^{
    id state = fc_nstimer_mutation_begin(timer, loop, mode);
    @try { body(); }
    @finally { fc_nstimer_mutation_end(state, timer); }
  });
}
typedef NSTimer *(*BlockFactory)(id, SEL, NSTimeInterval, BOOL, void (^)(NSTimer *));
typedef NSTimer *(*TargetFactory)(id, SEL, NSTimeInterval, id, SEL, id, BOOL);
static BlockFactory original_block, original_scheduled_block;
static TargetFactory original_target, original_scheduled_target;

@interface FCObservedTimerTarget : NSObject
@property(nonatomic, strong) id target;
@property(nonatomic) SEL selector;
- (void)fire:(NSTimer *)timer;
@end
@implementation FCObservedTimerTarget
- (void)fire:(NSTimer *)timer {
  fc_observe_runloop_callback(timer, ^{
    ((void (*)(id, SEL, NSTimer *))objc_msgSend)(self.target, self.selector, timer);
  });
}
@end

static NSTimer *make_block_timer(BlockFactory original, id cls, SEL selector, NSTimeInterval interval,
                                  BOOL repeats, void (^block)(NSTimer *)) {
  __block NSTimer *result;
  fc_observe_callback(^{
    result = original(cls, selector, interval, repeats, ^(NSTimer *timer) {
      fc_observe_runloop_callback(timer, ^{ block(timer); });
    });
    fc_nstimer_created(result);
  });
  return result;
}
static NSTimer *block_timer(id cls, SEL selector, NSTimeInterval interval, BOOL repeats,
                            void (^block)(NSTimer *)) {
  return make_block_timer(original_block, cls, selector, interval, repeats, block);
}
static NSTimer *scheduled_block_timer(id cls, SEL selector, NSTimeInterval interval, BOOL repeats,
                                      void (^block)(NSTimer *)) {
  return make_block_timer(original_scheduled_block, cls, selector, interval, repeats, block);
}
static NSTimer *make_target_timer(TargetFactory original, id cls, SEL selector, NSTimeInterval interval,
                                   id target, SEL action, id info, BOOL repeats) {
  FCObservedTimerTarget *proxy = [FCObservedTimerTarget new];
  proxy.target = target; proxy.selector = action;
  __block NSTimer *result;
  fc_observe_callback(^{
    result = original(cls, selector, interval, proxy, @selector(fire:), info, repeats);
    fc_nstimer_created(result);
  });
  return result;
}
static NSTimer *target_timer(id cls, SEL selector, NSTimeInterval interval, id target,
                             SEL action, id info, BOOL repeats) {
  return make_target_timer(original_target, cls, selector, interval, target, action, info, repeats);
}
static NSTimer *scheduled_target_timer(id cls, SEL selector, NSTimeInterval interval, id target,
                                       SEL action, id info, BOOL repeats) {
  return make_target_timer(original_scheduled_target, cls, selector, interval, target, action, info, repeats);
}

// Mutations can release application captures or read an application date object.
// Count them before native entry; never hold the activity mutex across the call.
static void (*original_invalidate)(id, SEL), (*original_fire)(id, SEL);
static void (*original_fire_date)(id, SEL, NSDate *);
static void timer_invalidate(id timer, SEL selector) {
  mutate_timer(timer, NULL, NULL, ^{ original_invalidate(timer, selector); });
}
static void timer_fire_date(id timer, SEL selector, NSDate *date) {
  mutate_timer(timer, NULL, NULL, ^{ original_fire_date(timer, selector, date); });
}
static void timer_fire(id timer, SEL selector) {
  mutate_timer(timer, NULL, NULL, ^{ original_fire(timer, selector); });
}
static IMP install_timer_method(Class cls, SEL selector, IMP replacement) {
  Method method = class_getInstanceMethod(cls, selector);
  if (!method) abort();
  IMP original = method_getImplementation(method);
  if (!class_addMethod(cls, selector, replacement, method_getTypeEncoding(method)))
    method_setImplementation(class_getInstanceMethod(cls, selector), replacement);
  return original;
}

#define INTERPOSE(replacement, original) \
  __attribute__((used)) static struct { const void *r; const void *o; } interpose_##original \
  __attribute__((section("__DATA,__interpose"))) = {(const void *)&replacement, (const void *)&original};
extern int fc_manual_clock_enabled(void);
static void timer_add(CFRunLoopRef loop, CFRunLoopTimerRef timer, CFRunLoopMode mode) {
  if (!fc_manual_clock_enabled()) { CFRunLoopAddTimer(loop, timer, mode); return; }
  mutate_timer((__bridge NSTimer *)timer, loop, mode, ^{ CFRunLoopAddTimer(loop, timer, mode); });
}
static void timer_remove(CFRunLoopRef loop, CFRunLoopTimerRef timer, CFRunLoopMode mode) {
  if (!fc_manual_clock_enabled()) { CFRunLoopRemoveTimer(loop, timer, mode); return; }
  mutate_timer((__bridge NSTimer *)timer, loop, mode, ^{ CFRunLoopRemoveTimer(loop, timer, mode); });
}
static void timer_change(CFRunLoopTimerRef timer, CFAbsoluteTime date) {
  if (!fc_manual_clock_enabled()) { CFRunLoopTimerSetNextFireDate(timer, date); return; }
  mutate_timer((__bridge NSTimer *)timer, NULL, NULL, ^{ CFRunLoopTimerSetNextFireDate(timer, date); });
}
static void timer_cancel(CFRunLoopTimerRef timer) {
  if (!fc_manual_clock_enabled()) { CFRunLoopTimerInvalidate(timer); return; }
  mutate_timer((__bridge NSTimer *)timer, NULL, NULL, ^{ CFRunLoopTimerInvalidate(timer); });
}
INTERPOSE(timer_add, CFRunLoopAddTimer)
INTERPOSE(timer_remove, CFRunLoopRemoveTimer)
INTERPOSE(timer_change, CFRunLoopTimerSetNextFireDate)
INTERPOSE(timer_cancel, CFRunLoopTimerInvalidate)

@interface FCInstallTimerObservation : NSObject
@end
@implementation FCInstallTimerObservation
+ (void)load {
  if (!getenv("FC_MANUAL")) return;
  // Load before the fixture creates timers; never mutate runtime methods during a measurement.
  Method method = class_getClassMethod(NSTimer.class, @selector(timerWithTimeInterval:repeats:block:));
  if (!method) abort();
  original_block = (BlockFactory)method_setImplementation(method, (IMP)block_timer);
  method = class_getClassMethod(NSTimer.class, @selector(scheduledTimerWithTimeInterval:repeats:block:));
  if (!method) abort();
  original_scheduled_block = (BlockFactory)method_setImplementation(method, (IMP)scheduled_block_timer);
  method = class_getClassMethod(NSTimer.class, @selector(timerWithTimeInterval:target:selector:userInfo:repeats:));
  if (!method) abort();
  original_target = (TargetFactory)method_setImplementation(method, (IMP)target_timer);
  method = class_getClassMethod(NSTimer.class, @selector(scheduledTimerWithTimeInterval:target:selector:userInfo:repeats:));
  if (!method) abort();
  original_scheduled_target = (TargetFactory)method_setImplementation(method, (IMP)scheduled_target_timer);
  @autoreleasepool {
    // Discover the concrete native class; do not patch an inherited superclass method.
    NSTimer *probe = original_block(NSTimer.class, @selector(timerWithTimeInterval:repeats:block:),
                                   1, NO, ^(NSTimer *timer) {});
    Class cls = object_getClass(probe);
    original_fire = (void *)install_timer_method(cls, @selector(fire), (IMP)timer_fire);
    original_invalidate = (void *)install_timer_method(cls, @selector(invalidate), (IMP)timer_invalidate);
    original_fire_date = (void *)install_timer_method(cls, @selector(setFireDate:), (IMP)timer_fire_date);
  }
}
@end
