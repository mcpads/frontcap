// Hold each animation position until the host explicitly requests the next step.
#import <UIKit/UIKit.h>
#include <fcntl.h>
#include <unistd.h>
@interface App : UIResponder <UIApplicationDelegate>
@property(nonatomic, strong) UIWindow *window;
@property(nonatomic, strong) UIView *held;
@property(nonatomic, strong) UIView *reference;
@property(nonatomic) CFTimeInterval begin;
@property(nonatomic, strong) dispatch_source_t commands;
@property(nonatomic, copy) NSString *lastCommand;
@end
@implementation App
- (void)acknowledge:(NSString *)phase {
  NSString *path = [NSHomeDirectory() stringByAppendingPathComponent:@"Documents/fade-state.json"];
  NSData *data = [NSJSONSerialization dataWithJSONObject:@{@"phase":phase, @"width":@(self.window.bounds.size.width)} options:0 error:nil];
  if (![data writeToFile:path atomically:YES]) abort();
}
- (BOOL)application:(UIApplication *)app didFinishLaunchingWithOptions:(NSDictionary *)options {
  self.window = [[UIWindow alloc] initWithFrame:UIScreen.mainScreen.bounds];
  UIViewController *vc = [UIViewController new];
  vc.view.backgroundColor = UIColor.blackColor;
  self.window.rootViewController = vc;
  [self.window makeKeyAndVisible];
  self.held = [[UIView alloc] initWithFrame:vc.view.bounds];
  [vc.view addSubview:self.held];
  UIView *square = [[UIView alloc] initWithFrame:CGRectMake(50, 200, 200, 200)];
  self.reference = [[UIView alloc] initWithFrame:CGRectMake(50, 500, 200, 200)];
  for (UIView *v in @[square, self.reference]) { v.backgroundColor = UIColor.whiteColor; v.alpha = 0; }
  [self.held addSubview:square];
  [vc.view addSubview:self.reference];
  dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 2 * NSEC_PER_SEC), dispatch_get_main_queue(), ^{
    self.held.layer.speed = 0;
    self.held.layer.timeOffset = 0;
    [CATransaction flush];
    [UIView animateWithDuration:4 delay:0 options:UIViewAnimationOptionCurveLinear animations:^{ square.alpha = 1; } completion:nil];
    [CATransaction flush];
    CAAnimation *animation = [square.layer animationForKey:@"opacity"];
    if (!animation) abort();
    self.begin = animation.beginTime;
    NSString *documents = [NSHomeDirectory() stringByAppendingPathComponent:@"Documents"];
    int fd = open(documents.fileSystemRepresentation, O_EVTONLY);
    if (fd < 0) abort();
    self.commands = dispatch_source_create(DISPATCH_SOURCE_TYPE_VNODE, fd, DISPATCH_VNODE_WRITE, dispatch_get_main_queue());
    dispatch_source_set_event_handler(self.commands, ^{
      NSData *data = [NSData dataWithContentsOfFile:[documents stringByAppendingPathComponent:@"fade-command.json"]];
      if (!data) return;
      NSDictionary *command = [NSJSONSerialization JSONObjectWithData:data options:0 error:nil];
      NSString *phase = command[@"phase"];
      if (!phase || [phase isEqualToString:self.lastCommand]) return;
      self.lastCommand = phase;
      [self apply:phase];
    });
    dispatch_source_set_cancel_handler(self.commands, ^{ close(fd); });
    dispatch_resume(self.commands);
    [self acknowledge:@"ready"];
  });
  return YES;
}
- (void)apply:(NSString *)phase {
  if ([phase isEqualToString:@"step1"] || [phase isEqualToString:@"step3"]) {
    self.held.layer.timeOffset = self.begin + ([phase isEqualToString:@"step1"] ? 1 : 3);
  } else if ([phase isEqualToString:@"reference"]) {
    [UIView animateWithDuration:4 delay:0 options:UIViewAnimationOptionCurveLinear
                     animations:^{ self.reference.alpha = 1; } completion:nil];
  } else abort();
  [CATransaction flush];
  [self acknowledge:phase];
}
@end
int main(int argc, char **argv) {
  @autoreleasepool { return UIApplicationMain(argc, argv, nil, @"App"); }
}
