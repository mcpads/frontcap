#import "scroll_input.h"
#import "clock.h"
#import "text_control.h"
#import "../../spikes/s18-native-time/ios/clock_control.h"
#include <dlfcn.h>
#include <errno.h>

static uint64_t (*instant)(void), (*limit)(void);
static int (*track)(void), (*advance)(uint64_t, uint64_t), (*drain)(uint64_t);
static NSDictionary *endpoint;
NSDictionary *start_native_clock(NSDictionary *(^metadata)(void), NSArray<NSString *> *diagnosticFiles) {
  if (!getenv("FC_MANUAL")) return nil;
  if (endpoint) return endpoint;
  instant = dlsym(RTLD_DEFAULT, "fc_manual_now_ns");
  limit = dlsym(RTLD_DEFAULT, "fc_manual_advance_limit_ns");
  track = dlsym(RTLD_DEFAULT, "fc_dispatch_tracking_start");
  advance = dlsym(RTLD_DEFAULT, "fc_dispatch_advance_when_idle");
  drain = dlsym(RTLD_DEFAULT, "fc_dispatch_wait_idle");
  if (!instant || !limit || !track || !advance || !drain) abort();
  endpoint = start_clock_control(^NSDictionary *(NSDictionary *request) {
    uint64_t before = instant();
    if ([request[@"operation"] isEqual:@"read"]) {
      NSMutableDictionary *result = [@{@"before_ns":@(before), @"now_ns":@(before), @"applied":@NO,
        @"advance_result":NSNull.null, @"drain_result":NSNull.null} mutableCopy];
      if (metadata) [result addEntriesFromDictionary:metadata()];
      return result;
    }
    uint64_t delta = [request[@"delta_ns"] unsignedLongLongValue];
    int stepped = delta > limit() ? EOVERFLOW : advance(delta, 2000000000);
    int drained = stepped ? 0 : drain(2000000000);
    NSDictionary *result = @{@"status":(stepped || drained) ? @"failed" : @"ok", @"before_ns":@(before), @"now_ns":@(instant()),
      @"applied":stepped ? @NO : @YES, @"advance_result":@(stepped), @"drain_result":stepped ? NSNull.null : @(drained)};
    if (stepped || drained) {
      // Preserve this generation before the next XCTest app launch replaces shared diagnostics.
      NSString *documents = [NSHomeDirectory() stringByAppendingPathComponent:@"Documents"];
      NSString *name = [NSString stringWithFormat:@"clock-failure-%@-%@", endpoint[@"generation"], request[@"request_id"]];
      NSString *directory = [documents stringByAppendingPathComponent:name];
      NSFileManager *files = NSFileManager.defaultManager;
      if ([files createDirectoryAtPath:directory withIntermediateDirectories:YES attributes:nil error:NULL]) {
        NSMutableDictionary *copies = [NSMutableDictionary new];
        NSArray *names = diagnosticFiles;
        for (NSString *file in names) {
          NSError *error;
          BOOL copied = [files copyItemAtPath:[documents stringByAppendingPathComponent:file]
                                      toPath:[directory stringByAppendingPathComponent:file] error:&error];
          copies[file] = copied ? @"copied" : error.localizedDescription ?: @"copy failed";
        }
        NSDictionary *failure = @{@"endpoint":endpoint, @"request":request, @"result":result, @"files":copies};
        [[NSJSONSerialization dataWithJSONObject:failure options:NSJSONWritingPrettyPrinted error:NULL]
          writeToFile:[directory stringByAppendingPathComponent:@"failure.json"] atomically:YES];
      }
    }
    return result;
  }, ^uint64_t { return instant(); });
  return endpoint;
}
void begin_native_clock_tracking(void) {
  static BOOL started;
  if (track && !started) { if (track()) abort(); started = YES; }
}

@interface FCClockWindow ()
@property(nonatomic, strong) NSDictionary *textEndpoint;
@property(nonatomic, strong) NSMutableArray *inputEvents;
@property(nonatomic, strong) NSDictionary *inputBoundary;
@property(nonatomic, strong) NSMutableArray *scrollObservations;
@property(nonatomic) uint64_t inputSerial;
@end
@implementation FCClockWindow
- (void)sendEvent:(UIEvent *)event {
  BOOL inspect = getenv("FC_UI_INSPECT") || getenv("FC_UI_EDIT");
  NSMutableArray *touches = [NSMutableArray new];
  if (inspect) for (UITouch *touch in event.allTouches) {
    CGPoint point = [touch locationInView:self];
    UIView *hit = [self hitTest:point withEvent:event];
    UIView *input = touch.view;
    NSUInteger depth = 0;
    while (input && ![input isKindOfClass:UITextField.class] && ![input isKindOfClass:UITextView.class]) {
      input = ++depth < 64 ? input.superview : nil;
    }
    [touches addObject:@{@"phase":@(touch.phase), @"timestamp":@(touch.timestamp),
      @"view":NSStringFromClass(touch.view.class) ?: @"", @"hit":NSStringFromClass(hit.class) ?: @"",
      @"identifier":touch.view.accessibilityIdentifier ?: @"", @"label":touch.view.accessibilityLabel ?: @"",
      @"input_identifier":input.accessibilityIdentifier ?: @"", @"input_label":input.accessibilityLabel ?: @"",
      @"input_role":input ? ([input isKindOfClass:UITextField.class] ? @"text_field" : @"text_view") : @"none",
      @"x":@(point.x), @"y":@(point.y)}];
  }
  [super sendEvent:event];
  if (inspect && touches.count) {
    if (!self.inputEvents) self.inputEvents = [NSMutableArray new];
    [self.inputEvents addObject:@{@"touches":touches, @"now_ns":instant ? @(instant()) : NSNull.null,
      @"text_input":inspect_uikit_text_input(self)}];
    if (self.inputEvents.count > 16) [self.inputEvents removeObjectAtIndex:0];
    if (self.inputBoundary) {
      NSDictionary *record = @{@"generation":endpoint[@"generation"], @"serial":@(++self.inputSerial),
        @"now_ns":@(instant()), @"touches":touches};
      NSData *data = [NSJSONSerialization dataWithJSONObject:record options:0 error:NULL];
      if (![data writeToFile:self.inputBoundary[@"path"] options:NSDataWritingAtomic error:NULL]) abort();
    }
  }
}
- (instancetype)initWithFrame:(CGRect)frame {
  if ((self = [super initWithFrame:frame]) && (getenv("FC_MANUAL") || getenv("FC_UI_INSPECT") || getenv("FC_UI_EDIT")))
    self.accessibilityIdentifier = @"frontcap-clock";
  return self;
}
- (instancetype)initWithWindowScene:(UIWindowScene *)scene {
  if ((self = [super initWithWindowScene:scene]) && (getenv("FC_MANUAL") || getenv("FC_UI_INSPECT") || getenv("FC_UI_EDIT")))
    self.accessibilityIdentifier = @"frontcap-clock";
  return self;
}
- (NSString *)accessibilityValue {
  if ((!endpoint || !instant) && !getenv("FC_UI_INSPECT") && !getenv("FC_UI_EDIT")) return [super accessibilityValue];
  NSMutableDictionary *value = [NSMutableDictionary new];
  if (endpoint && instant) [value addEntriesFromDictionary:@{@"endpoint":endpoint, @"now_ns":@(instant())}];
  if (getenv("FC_UI_INSPECT") || getenv("FC_UI_EDIT")) {
    if (endpoint && instant && !self.inputBoundary) {
      NSString *path = [[NSHomeDirectory() stringByAppendingPathComponent:@"Documents"]
        stringByAppendingPathComponent:[NSString stringWithFormat:@"input-%@.json", endpoint[@"generation"]]];
      self.inputBoundary = @{@"generation":endpoint[@"generation"], @"path":path};
      NSDictionary *initial = @{@"generation":endpoint[@"generation"], @"serial":@0,
        @"now_ns":@(instant()), @"touches":@[]};
      if (![[NSJSONSerialization dataWithJSONObject:initial options:0 error:NULL]
        writeToFile:path options:NSDataWritingAtomic error:NULL]) abort();
    }
    if (self.inputBoundary) value[@"input_boundary"] = self.inputBoundary;
    value[@"text_input"] = inspect_uikit_text_input(self);
    NSArray *scrolls=inspect_uikit_scrolls(self);
    if (scrolls) value[@"scrolls"]=scrolls;
    if (scrolls && endpoint && instant) {
      NSDictionary *row=@{@"app":endpoint,@"now_ns":@(instant()),@"scrolls":scrolls};
      if (![self.scrollObservations.lastObject isEqual:row]) {
        if (!self.scrollObservations) self.scrollObservations=[NSMutableArray new];
        [self.scrollObservations addObject:row];
        if (self.scrollObservations.count>128) [self.scrollObservations removeObjectAtIndex:0];
        NSData *trace=[NSJSONSerialization dataWithJSONObject:self.scrollObservations options:0 error:NULL];
        BOOL written=[trace writeToFile:[NSHomeDirectory() stringByAppendingPathComponent:@"Documents/scroll-state.json"] options:NSDataWritingAtomic error:NULL];
        if (!written) value[@"scroll_trace_error"]=@"write_failed";
      }
    }
    value[@"transitioning"] = uikit_transition_in_progress(self) ? @YES : @NO;
    value[@"input_diagnostics"] = @{@"events":self.inputEvents ?: @[],
      @"ignoring_interaction":UIApplication.sharedApplication.isIgnoringInteractionEvents ? @YES : @NO,
      @"window_interaction":self.userInteractionEnabled ? @YES : @NO,
      @"root_interaction":self.rootViewController.view.userInteractionEnabled ? @YES : @NO};
  }
  if (getenv("FC_UI_EDIT")) {
    if (!self.textEndpoint) self.textEndpoint = start_uikit_text_control(self);
    value[@"text_endpoint"] = self.textEndpoint;
    value[@"text_receipt"] = @{@"generation":self.textEndpoint[@"generation"],
      @"path":[NSHomeDirectory() stringByAppendingPathComponent:@"Documents/native-text/text-control.json"]};
  }
  NSData *data = [NSJSONSerialization dataWithJSONObject:value options:0 error:NULL];
  return [[NSString alloc] initWithData:data encoding:NSUTF8StringEncoding];
}
@end
