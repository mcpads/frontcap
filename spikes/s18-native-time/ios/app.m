#import <UIKit/UIKit.h>
#include "dispatch_activity.h"
#import "clock_control.h"
#include <dlfcn.h>
#include <pthread.h>
#include <unistd.h>
#include <fcntl.h>
#include <mach/mach_time.h>

static int (*track)(void), (*step)(uint64_t, uint64_t), (*wait_idle)(uint64_t);
static int (*schedule)(uint64_t, dispatch_queue_t, dispatch_block_t);
static uint64_t (*instant)(void), (*advance_limit)(void);
static uint64_t (*real_abs)(void);
static void (*read_activity)(struct fc_dispatch_activity *);
static double uptime_origin, wall_origin;
static uint64_t mach_origin;
static mach_timebase_info_data_t app_timebase;
static double media_origin, last_model_time, last_media_time;
static uint64_t update_phases[4];
static NSDictionary *control_endpoint;
static NSDictionary *clock_operation(NSDictionary *request);

static NSString *input_log_path(void) {
  return [NSHomeDirectory() stringByAppendingPathComponent:getenv("FC_MANUAL")
    ? @"Documents/input-events.txt" : @"Documents/input-events-real.txt"];
}
static void log_input(NSString *line) {
  NSString *path = input_log_path();
  if (![[NSFileManager defaultManager] fileExistsAtPath:path]) [@"" writeToFile:path atomically:YES encoding:NSUTF8StringEncoding error:NULL];
  NSFileHandle *file = [NSFileHandle fileHandleForWritingAtPath:path];
  [file seekToEndOfFile]; [file writeData:[[line stringByAppendingString:@"\n"] dataUsingEncoding:NSUTF8StringEncoding]]; [file closeFile];
}
@interface ProbeApplication : UIApplication
@end
@implementation ProbeApplication
- (void)sendEvent:(UIEvent *)event {
  if (getenv("FC_TRACE_UI")) log_input([NSString stringWithFormat:@"application event type=%ld touches=%lu",
    (long)event.type, (unsigned long)event.allTouches.count]);
  [super sendEvent:event];
}
@end
@interface ClockLabel : UILabel
@end
@implementation ClockLabel
- (NSString *)accessibilityValue {
  NSDictionary *state = @{@"now_ns":@(instant()), @"control":control_endpoint ?: @{},
    @"uptime_s":@(NSProcessInfo.processInfo.systemUptime - uptime_origin),
    @"wall_s":@(NSDate.date.timeIntervalSince1970 - wall_origin),
    @"mach_s":@((double)(mach_absolute_time() - mach_origin) * app_timebase.numer / app_timebase.denom / 1e9),
    @"ui_phases":@[@(update_phases[0]), @(update_phases[1]), @(update_phases[2]), @(update_phases[3]) ],
    @"update_timing":@{@"model_s":@(last_model_time - media_origin),
                       @"app_media_s":@(last_media_time - media_origin)}};
  NSData *data = [NSJSONSerialization dataWithJSONObject:state options:0 error:NULL];
  return [[NSString alloc] initWithData:data encoding:NSUTF8StringEncoding];
}
@end
@interface ProbeButton : UIButton
@end
@implementation ProbeButton
- (void)touchesBegan:(NSSet<UITouch *> *)touches withEvent:(UIEvent *)event {
  log_input(@"button touches began"); [super touchesBegan:touches withEvent:event];
}
- (void)touchesEnded:(NSSet<UITouch *> *)touches withEvent:(UIEvent *)event {
  log_input(@"button touches ended"); [super touchesEnded:touches withEvent:event];
}
- (void)sendAction:(SEL)action to:(id)target forEvent:(UIEvent *)event {
  log_input([@"button action " stringByAppendingString:NSStringFromSelector(action)]); [super sendAction:action to:target forEvent:event];
}
@end
@interface ProbeWindow : UIWindow
@end
@implementation ProbeWindow
- (void)sendEvent:(UIEvent *)event {
  for (UITouch *touch in event.allTouches) {
    NSString *line = [NSString stringWithFormat:@"phase=%ld timestamp=%.6f point=%@ view=%@\n",
      (long)touch.phase, touch.timestamp, NSStringFromCGPoint([touch locationInView:self]), touch.view];
    log_input(line);
  }
  [super sendEvent:event];
  for (UITouch *touch in event.allTouches) {
    log_input([NSString stringWithFormat:@"input gestures=%@", touch.view.gestureRecognizers]);
    for (UIGestureRecognizer *gesture in touch.view.gestureRecognizers)
      log_input([NSString stringWithFormat:@"gesture %@ state=%ld delays=%d/%d cancels=%d",
        NSStringFromClass(gesture.class), (long)gesture.state, gesture.delaysTouchesBegan,
        gesture.delaysTouchesEnded, gesture.cancelsTouchesInView]);
  }
}
@end
@interface NativeProbeApp : UIResponder <UIApplicationDelegate>
@property(nonatomic, strong) UIWindow *window;
@property(nonatomic, strong) UILabel *label;
@property(nonatomic, strong) UIView *color;
@property(nonatomic) NSInteger count;
@property(nonatomic, strong) UILabel *clock;
@property(nonatomic, strong) UIUpdateLink *updateLink;
@property(nonatomic, strong) dispatch_queue_t heldQueue;
@property(nonatomic, strong) dispatch_semaphore_t heldCallback;
@property(nonatomic) BOOL holdRunning;
@end
@implementation NativeProbeApp
- (BOOL)application:(UIApplication *)application didFinishLaunchingWithOptions:(NSDictionary *)options {
  self.window = [[ProbeWindow alloc] initWithFrame:UIScreen.mainScreen.bounds];
  UIViewController *controller = [UIViewController new];
  controller.view.backgroundColor = UIColor.whiteColor;
  self.window.rootViewController = controller;
  self.label = [[UILabel alloc] initWithFrame:CGRectMake(32, 120, 300, 50)];
  self.label.font = [UIFont systemFontOfSize:28];
  self.label.textColor = UIColor.blackColor;
  self.label.text = @"Count: 0";
  self.label.accessibilityIdentifier = @"native-count";
  [controller.view addSubview:self.label];
  self.color = [[UIView alloc] initWithFrame:CGRectMake(32, 190, 160, 100)];
  self.color.backgroundColor = UIColor.redColor;
  self.color.isAccessibilityElement = YES;
  self.color.accessibilityIdentifier = @"native-color";
  self.color.accessibilityLabel = @"State color";
  [controller.view addSubview:self.color];
  UIButton *button = [ProbeButton buttonWithType:UIButtonTypeSystem];
  button.frame = CGRectMake(32, 320, 300, 60);
  [button setTitle:@"Increment" forState:UIControlStateNormal];
  button.accessibilityIdentifier = @"native-increment";
  [button addTarget:self action:@selector(increment) forControlEvents:UIControlEventTouchUpInside];
  [controller.view addSubview:button];
  if (getenv("FC_MANUAL")) {
    track = dlsym(RTLD_DEFAULT, "fc_dispatch_tracking_start");
    step = dlsym(RTLD_DEFAULT, "fc_dispatch_advance_when_idle");
    wait_idle = dlsym(RTLD_DEFAULT, "fc_dispatch_wait_idle");
    schedule = dlsym(RTLD_DEFAULT, "fc_dispatch_after_ns");
    instant = dlsym(RTLD_DEFAULT, "fc_manual_now_ns");
    advance_limit = dlsym(RTLD_DEFAULT, "fc_manual_advance_limit_ns");
    real_abs = dlsym(RTLD_DEFAULT, "fc_real_abs");
    read_activity = dlsym(RTLD_DEFAULT, "fc_dispatch_activity_read");
    if (!track || !step || !wait_idle || !schedule || !instant || !advance_limit || !real_abs || !read_activity) abort();
    uptime_origin = NSProcessInfo.processInfo.systemUptime;
    wall_origin = NSDate.date.timeIntervalSince1970;
    media_origin = CACurrentMediaTime();
    mach_origin = mach_absolute_time();
    mach_timebase_info(&app_timebase);
    self.clock = [[ClockLabel alloc] initWithFrame:CGRectMake(32, 400, 340, 50)];
    self.clock.text = @"Time: 0";
    self.clock.accessibilityIdentifier = @"native-clock";
    [controller.view addSubview:self.clock];
    control_endpoint = start_clock_control(^NSDictionary *(NSDictionary *request) {
      return clock_operation(request);
    }, ^uint64_t { return instant(); });
    UIButton *due = [ProbeButton buttonWithType:UIButtonTypeSystem];
    due.frame = CGRectMake(32, 470, 300, 60);
    [due setTitle:@"Hold due work" forState:UIControlStateNormal];
    due.accessibilityIdentifier = @"native-hold-due";
    [due addTarget:self action:@selector(holdDueWork) forControlEvents:UIControlEventTouchUpInside];
    [controller.view addSubview:due];
    UIButton *hold = [ProbeButton buttonWithType:UIButtonTypeSystem];
    hold.frame = CGRectMake(32, 680, 300, 60);
    [hold setTitle:@"Hold work" forState:UIControlStateNormal];
    hold.accessibilityIdentifier = @"native-hold-work";
    [hold addTarget:self action:@selector(toggleWork:) forControlEvents:UIControlEventTouchUpInside];
    [controller.view addSubview:hold];
  }
  [self.window makeKeyAndVisible];
  if (getenv("FC_TRACE_UI")) {
    NSArray *phases = @[UIUpdateActionPhase.afterUpdateScheduled, UIUpdateActionPhase.beforeEventDispatch,
      UIUpdateActionPhase.afterEventDispatch, UIUpdateActionPhase.afterUpdateComplete];
    NSArray *names = @[@"scheduled", @"before-events", @"after-events", @"complete"];
    self.updateLink = [UIUpdateLink updateLinkForView:controller.view];
    for (NSUInteger i = 0; i < phases.count; i++) {
      [self.updateLink addActionToPhase:phases[i] handler:^(UIUpdateLink *link, UIUpdateInfo *info) {
        update_phases[i]++;
        last_model_time = info.modelTime;
        last_media_time = CACurrentMediaTime();
        if (update_phases[i] <= 4) log_input([NSString stringWithFormat:
          @"update %@ model=%.9f deadline=%.9f media=%.9f", names[i], info.modelTime,
          info.completionDeadlineTime, CACurrentMediaTime()]);
      }];
    }
    // Passive observation only: do not request continuous updates or low-latency dispatch.
    self.updateLink.enabled = YES;
  }
  return YES;
}
- (void)applicationDidBecomeActive:(UIApplication *)application { log_input(@"application active"); }
- (void)holdDueWork {
  if (self.heldQueue || self.heldCallback) abort();
  self.heldQueue = dispatch_queue_create("dev.frontcap.fixture.due", DISPATCH_QUEUE_SERIAL);
  dispatch_suspend(self.heldQueue);
  if (schedule(1000000, self.heldQueue, ^{ log_input(@"due work drained"); })) abort();
}
- (void)toggleWork:(UIButton *)button {
  if (self.heldCallback) {
    dispatch_semaphore_signal(self.heldCallback);
    self.heldCallback = nil;
    [button setTitle:@"Hold work" forState:UIControlStateNormal];
  } else if (self.heldQueue) {
    dispatch_resume(self.heldQueue);
    self.heldQueue = nil;
    self.holdRunning = YES;
    [button setTitle:@"Hold work" forState:UIControlStateNormal];
  } else if (self.holdRunning) {
    dispatch_semaphore_t release = dispatch_semaphore_create(0);
    self.heldCallback = release;
    dispatch_async(dispatch_queue_create("dev.frontcap.fixture.running", DISPATCH_QUEUE_SERIAL), ^{
      dispatch_async(dispatch_get_main_queue(), ^{ [button setTitle:@"Release work" forState:UIControlStateNormal]; });
      dispatch_semaphore_wait(release, DISPATCH_TIME_FOREVER);
      dispatch_async(dispatch_get_main_queue(), ^{ log_input(@"running work drained"); });
    });
  } else {
    self.heldQueue = dispatch_queue_create("dev.frontcap.fixture.held", DISPATCH_QUEUE_SERIAL);
    dispatch_suspend(self.heldQueue);
    dispatch_async(self.heldQueue, ^{ dispatch_async(dispatch_get_main_queue(), ^{ log_input(@"held work drained"); }); });
    [button setTitle:@"Release work" forState:UIControlStateNormal];
  }
}
- (void)increment {
  if (schedule) {
    if (self.count || [self.label.text isEqualToString:@"Pending"]) return;
    if (track()) abort();
    self.label.text = @"Pending";
    if (schedule(5000000000, dispatch_get_main_queue(), ^{
      dispatch_async(dispatch_get_global_queue(0, 0), ^{
        dispatch_async(dispatch_get_main_queue(), ^{ [self applyIncrement]; });
      });
    })) abort();
  } else [self applyIncrement];
}
- (void)applyIncrement {
  self.count++;
  self.label.text = [NSString stringWithFormat:@"Count: %ld", (long)self.count];
  self.color.backgroundColor = UIColor.greenColor;
}
@end
static NSDictionary *clock_operation(NSDictionary *request) {
  uint64_t before_ns = instant();
  if ([request[@"operation"] isEqual:@"read"]) return @{@"status":@"ok", @"applied":@NO,
    @"before_ns":@(before_ns), @"now_ns":@(before_ns), @"advance_result":NSNull.null, @"drain_result":NSNull.null};
  uint64_t delta = [request[@"delta_ns"] unsignedLongLongValue];
  struct fc_dispatch_activity before;
  read_activity(&before);
  uint64_t start = real_abs();
  // Bound the external protocol to signed nanoseconds, including each initial clock base.
  uint64_t limit = advance_limit();
  int advance_result = delta > limit ? EOVERFLOW : step(delta, 2000000000);
  uint64_t advanced = real_abs();
  if (!advance_result) clock_control_checkpoint(@"after-advance", request);
  int drain_result = advance_result ? 0 : wait_idle(2000000000);
  uint64_t finished = real_abs();
  int result = advance_result ?: drain_result;
  struct fc_dispatch_activity activity;
  read_activity(&activity);
  uint64_t advance_wait_ns = (uint64_t)((__uint128_t)(advanced - start) * app_timebase.numer / app_timebase.denom);
  uint64_t drain_wait_ns = (uint64_t)((__uint128_t)(finished - advanced) * app_timebase.numer / app_timebase.denom);
  NSDictionary *row = @{@"pid":@(getpid()), @"request":request, @"delta_ns":@(delta),
    @"before_ns":@(before_ns), @"now_ns":@(instant()), @"result":@(result),
    @"status":result ? @"failed" : @"ok", @"applied":advance_result ? @NO : @YES,
    @"advance_result":@(advance_result), @"drain_result":advance_result ? NSNull.null : @(drain_result),
    @"submitted":@(activity.submitted), @"completed":@(activity.completed), @"pending":@(activity.pending),
    @"before_pending":@(before.pending), @"advance_wait_ns":@(advance_wait_ns), @"drain_wait_ns":@(drain_wait_ns)};
  NSString *path = [NSHomeDirectory() stringByAppendingPathComponent:@"Documents/clock-advances.jsonl"];
  NSMutableData *data = [[NSJSONSerialization dataWithJSONObject:row options:0 error:NULL] mutableCopy];
  [data appendBytes:"\n" length:1];
  int fd = open(path.fileSystemRepresentation, O_CREAT | O_WRONLY | O_APPEND, 0600);
  if (fd < 0 || write(fd, data.bytes, data.length) != data.length || close(fd)) abort();
  dispatch_async(dispatch_get_main_queue(), ^{
    NativeProbeApp *app = (NativeProbeApp *)UIApplication.sharedApplication.delegate;
    app.clock.text = result ? [NSString stringWithFormat:@"Error: %d", result]
                            : [NSString stringWithFormat:@"Time: %llu", instant()];
  });
  return row;
}
int main(int argc, char **argv) {
  @autoreleasepool {
    NSString *path = input_log_path();
    if (getenv("FC_MANUAL")) [[NSFileManager defaultManager] removeItemAtPath:
      [NSHomeDirectory() stringByAppendingPathComponent:@"Documents/clock-advances.jsonl"] error:NULL];
    [[NSString stringWithFormat:@"mode=%s\n", getenv("FC_MANUAL") ? "manual" : "real"]
      writeToFile:path atomically:YES encoding:NSUTF8StringEncoding error:NULL];
    return UIApplicationMain(argc, argv, NSStringFromClass(ProbeApplication.class), NSStringFromClass(NativeProbeApp.class));
  }
}
