#pragma once
#import <UIKit/UIKit.h>
#import <objc/runtime.h>

// UIKit can advertise XCTest hittability before a navigation transition accepts input.
static BOOL uikit_transition_in_progress(UIWindow *window) {
  if (!NSThread.isMainThread || !window.rootViewController) return YES;
  NSMutableArray<UIViewController *> *pending = [NSMutableArray arrayWithObject:window.rootViewController];
  NSUInteger visited = 0;
  while (pending.count) {
    UIViewController *controller = pending.lastObject; [pending removeLastObject];
    if (++visited > 2048 || controller.transitionCoordinator) return YES;
    [pending addObjectsFromArray:controller.childViewControllers];
    if (controller.presentedViewController) [pending addObject:controller.presentedViewController];
  }
  return NO;
}

// Main-thread, source-owned test inspection. UI focus from XCTest is not keyboard first-responder state.
static UIView *focused_uikit_view(UIWindow *window, NSString **reason) {
  if (!NSThread.isMainThread) { *reason = @"wrong_thread"; return nil; }
  NSMutableArray<UIView *> *pending = [NSMutableArray arrayWithObject:window];
  UIView *focused = nil;
  NSUInteger visited = 0;
  while (pending.count) {
    UIView *view = pending.lastObject; [pending removeLastObject];
    if (++visited > 2048) { *reason = @"view_budget"; return nil; }
    if (view.isFirstResponder) { focused = view; break; }
    [pending addObjectsFromArray:view.subviews];
  }
  return focused;
}
static NSDictionary *inspect_uikit_text_input(UIWindow *window) {
  NSString *reason;
  UIView *focused = focused_uikit_view(window, &reason);
  if (reason) return @{@"state":@"unavailable", @"reason":reason};
  if (!focused) return @{@"state":@"unfocused"};
  BOOL field = [focused isKindOfClass:UITextField.class];
  BOOL textView = [focused isKindOfClass:UITextView.class];
  if (!field && !textView) return @{@"state":@"unsupported", @"reason":@"input_type"};
  id<UITextInput> input = (id<UITextInput>)focused;
  if (((id<UITextInputTraits>)focused).secureTextEntry)
    return @{@"state":@"unsupported", @"reason":@"secure_text"};
  BOOL editable = field ? ((UITextField *)focused).enabled : ((UITextView *)focused).editable;
  if (!editable) return @{@"state":@"unsupported", @"reason":@"read_only"};
  static char tokenKey;
  NSString *token = objc_getAssociatedObject(focused, &tokenKey);
  if (!token) {
    token = NSUUID.UUID.UUIDString.lowercaseString;
    objc_setAssociatedObject(focused, &tokenKey, token, OBJC_ASSOCIATION_COPY_NONATOMIC);
  }
  NSString *text = field ? ((UITextField *)focused).text : ((UITextView *)focused).text;
  if (text.length > 16384) return @{@"state":@"unavailable", @"reason":@"text_budget"};
  if (focused.accessibilityIdentifier.length > 4096 || focused.accessibilityLabel.length > 4096)
    return @{@"state":@"unavailable", @"reason":@"selector_budget"};
  UITextRange *selection = input.selectedTextRange;
  if (!selection) return @{@"state":@"unavailable", @"reason":@"selection_missing"};
  NSInteger start = [input offsetFromPosition:input.beginningOfDocument toPosition:selection.start];
  NSInteger end = [input offsetFromPosition:input.beginningOfDocument toPosition:selection.end];
  if (start < 0 || end < start || (NSUInteger)end > text.length)
    return @{@"state":@"unavailable", @"reason":@"selection_invalid"};
  return @{@"state":@"focused", @"token":token, @"role":field ? @"text_field" : @"text_view",
    @"identifier":focused.accessibilityIdentifier ?: @"", @"label":focused.accessibilityLabel ?: @"",
    @"text":text ?: @"", @"start":@(start), @"end":@(end),
    @"marked":input.markedTextRange ? @YES : @NO};
}

static BOOL uikit_text_boundary(NSString *text, NSUInteger index) {
  if (index > text.length) return NO;
  return index == 0 || index == text.length ||
    !(CFStringIsSurrogateHighCharacter([text characterAtIndex:index-1]) &&
      CFStringIsSurrogateLowCharacter([text characterAtIndex:index]));
}
// A dispatched insert may be transformed or rejected by the app's ordinary input delegate.
// Report actual state; never infer persistence or a business effect from this acknowledgement.
static NSDictionary *replace_uikit_text(UIWindow *window, NSDictionary *basis,
                                        NSUInteger start, NSUInteger end, NSString *text) {
  NSDictionary *before = inspect_uikit_text_input(window);
  NSString *reason;
  if (![before[@"state"] isEqual:@"focused"]) reason = @"input_unavailable";
  else if (![before isEqual:basis]) reason = @"input_changed";
  else if ([before[@"marked"] boolValue]) reason = @"composition_active";
  NSString *original = before[@"text"];
  if (!reason && (start > end || !uikit_text_boundary(original, start) || !uikit_text_boundary(original, end)))
    reason = @"invalid_range";
  if (!reason && (text.length > 16384 || original.length - (end-start) + text.length > 16384))
    reason = @"text_budget";
  if (reason) return @{@"input":@"not_applied", @"reason":reason, @"current":before};
  NSString *lookupError;
  id<UITextInput> input = (id<UITextInput>)focused_uikit_view(window, &lookupError);
  UITextPosition *from = [input positionFromPosition:input.beginningOfDocument offset:(NSInteger)start];
  UITextPosition *to = [input positionFromPosition:input.beginningOfDocument offset:(NSInteger)end];
  UITextRange *range = from && to ? [input textRangeFromPosition:from toPosition:to] : nil;
  if (!range) return @{@"input":@"not_applied", @"reason":@"range_unavailable", @"current":before};
  input.selectedTextRange = range;
  [input insertText:text];
  return @{@"input":@"uikit_text_input", @"before":before, @"after":inspect_uikit_text_input(window)};
}
