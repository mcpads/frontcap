// Installed controlled-input qualification. Consumer selectors stay in the test caller.
#import "observed_ui_checks.h"
#import "timed_press.h"
static NSDictionary *check_uikit_timed_text(XCUIApplication *app, NSString *field, NSString *openEditor, NSString *bodyField,
                                           uint64_t delta, NSString *directory) {
  NSDictionary *(^read)(void) = ^NSDictionary *{
    NSString *value = app.windows[@"frontcap-clock"].value;
    return [NSJSONSerialization JSONObjectWithData:[value dataUsingEncoding:NSUTF8StringEncoding] options:0 error:NULL];
  };
  NSCAssert([app.windows[@"frontcap-clock"] waitForExistenceWithTimeout:10], @"Clock window unavailable");
  NSDictionary *metadata = read(), *endpoint = metadata[@"endpoint"];
  if (openEditor) [app.buttons[openEditor] tap];
  NSDictionary *advance = @{@"generation":endpoint[@"generation"], @"request_id":@1,
    @"operation":@"advance", @"delta_ns":@500000000};
  NSDictionary *prepared = uikit_control_exchange(endpoint,advance);
  NSCAssert([prepared[@"status"] isEqual:@"ok"] && [prepared[@"now_ns"] isEqual:@500000000], @"Prepare clock failed");
  metadata = read();
  FCObservedUI *ui = [[FCObservedUI alloc] initWithApplication:app];
  NSDictionary *context = @{@"app":endpoint, @"now_ns":metadata[@"now_ns"], @"text_input":metadata[@"text_input"]};
  NSDictionary *snapshot = [ui observeWithContext:context];
  NSDictionary *target = observed_target(snapshot,field);
  NSCAssert(target && [target[@"unique"] boolValue] && [target[@"hittable"] boolValue], @"Input must be uniquely ready");
  NSDictionary *request = @{@"generation":endpoint[@"generation"], @"request_id":@2,
    @"operation":@"advance", @"delta_ns":@(delta)};
  NSMutableDictionary *report = [@{@"delta_ns":@(delta), @"prepare":prepared, @"before":metadata,
    @"clock_request":request, @"state":@"press_pending"} mutableCopy];
  NSString *path = [directory stringByAppendingPathComponent:[NSString stringWithFormat:@"timed-%llu.json",delta]];
  void (^save)(void) = ^{
    NSCAssert([[NSJSONSerialization dataWithJSONObject:report options:NSJSONWritingPrettyPrinted error:NULL]
      writeToFile:path options:NSDataWritingAtomic error:NULL], @"Cannot save input evidence");
  };
  save();
  NSDictionary *press = [ui pressReference:target[@"reference"] observation:snapshot[@"observation"]
    context:context endpoint:endpoint clockRequest:request boundary:metadata[@"input_boundary"]];
  metadata = read(); report[@"press"] = press; report[@"after"] = metadata; report[@"state"] = @"press_returned"; save();
  NSCAssert([press[@"input"] isEqual:@"xctest_timed_press"], @"Press delivery order not established");
  NSCAssert([metadata[@"now_ns"] unsignedLongLongValue] == 500000000+delta, @"Unexpected clock delta");
  NSDictionary *focus = metadata[@"text_input"];
  if (!delta) {
    NSCAssert([focus[@"state"] isEqual:@"unfocused"], @"Zero-time press control changed focus");
  } else {
    NSCAssert([focus[@"state"] isEqual:@"focused"] && [focus[@"text"] isEqual:@""], @"Timed press did not focus empty input");
    NSDictionary *textRequest = @{@"generation":metadata[@"text_endpoint"][@"generation"], @"request_id":@1,
      @"operation":@"replace_text", @"arguments":@{@"basis":focus, @"start":@0, @"end":@0, @"text":@"부산 여행 🌊"}};
    NSDictionary *edited = uikit_control_exchange(metadata[@"text_endpoint"],textRequest);
    report[@"edit"] = edited ?: NSNull.null; save();
    NSCAssert([edited[@"result"][@"input"] isEqual:@"uikit_text_input"], @"Committed input was not delivered");
    NSCAssert([app.textFields[field].value isEqual:@"부산 여행 🌊"], @"Independent UI text differs");
    [NSThread sleepForTimeInterval:.2];
    NSDictionary *held = read(); report[@"held"] = held; save();
    NSCAssert([held[@"now_ns"] isEqual:metadata[@"now_ns"]] &&
      [held[@"text_input"][@"text"] isEqual:@"부산 여행 🌊"], @"Held time/input changed");
  }
  if (delta && bodyField) {
    metadata = read();
    context = @{@"app":endpoint, @"now_ns":metadata[@"now_ns"], @"text_input":metadata[@"text_input"]};
    snapshot = [ui observeWithContext:context]; target = observed_target(snapshot,bodyField);
    NSCAssert(target && [target[@"hittable"] boolValue] && [target[@"unique"] boolValue], @"Body must be uniquely ready");
    NSDictionary *bodyRequest = @{@"generation":endpoint[@"generation"], @"request_id":@3,
      @"operation":@"advance", @"delta_ns":@(delta)};
    report[@"body_request"] = bodyRequest; report[@"state"] = @"body_press_pending"; save();
    NSDictionary *bodyPress = [ui pressReference:target[@"reference"] observation:snapshot[@"observation"]
      context:context endpoint:endpoint clockRequest:bodyRequest boundary:metadata[@"input_boundary"]];
    metadata = read(); report[@"body_press"] = bodyPress; report[@"body_after"] = metadata; save();
    NSCAssert([bodyPress[@"input"] isEqual:@"xctest_timed_press"], @"Body press ordering unavailable");
    NSDictionary *bodyFocus = metadata[@"text_input"];
    NSCAssert([bodyFocus[@"state"] isEqual:@"focused"] && [bodyFocus[@"role"] isEqual:@"text_view"] &&
      [bodyFocus[@"text"] isEqual:@""], @"Body did not receive editing focus");
    NSDictionary *bodyEdit = @{@"generation":metadata[@"text_endpoint"][@"generation"], @"request_id":@2,
      @"operation":@"replace_text", @"arguments":@{@"basis":bodyFocus, @"start":@0, @"end":@0, @"text":@"첫날 시장 산책"}};
    report[@"body_edit"] = uikit_control_exchange(metadata[@"text_endpoint"],bodyEdit) ?: NSNull.null; save();
    NSCAssert([report[@"body_edit"][@"result"][@"input"] isEqual:@"uikit_text_input"], @"Body edit failed");
    NSCAssert([app.textViews[bodyField].value isEqual:@"첫날 시장 산책"], @"Independent body text differs");
    NSDictionary *bodyHeld = read(); report[@"body_held"] = bodyHeld; save();
    NSCAssert([bodyHeld[@"now_ns"] isEqual:metadata[@"now_ns"]], @"Body edit advanced time");
  }
  report[@"state"] = @"passed"; save();
  NSString *image = [directory stringByAppendingPathComponent:[NSString stringWithFormat:@"timed-%llu.png",delta]];
  NSCAssert([app.screenshot.PNGRepresentation writeToFile:image atomically:YES], @"Cannot save press screenshot");
  return report;
}
