#pragma once
// One source text delivery. A lost reply can only read its exact published receipt.
#import "timed_press.h"

static NSDictionary *uikit_text_receipt(NSDictionary *descriptor) {
  NSData *data = [NSData dataWithContentsOfFile:descriptor[@"path"]];
  if (!data || data.length > 1024*1024) return nil;
  id value = [NSJSONSerialization JSONObjectWithData:data options:0 error:NULL];
  return [value isKindOfClass:NSDictionary.class] &&
    [value[@"generation"] isEqual:descriptor[@"generation"]] ? value : nil;
}
static NSDictionary *deliver_uikit_text(NSDictionary *endpoint, NSDictionary *descriptor, NSDictionary *request) {
  NSDictionary *before = uikit_text_receipt(descriptor);
  if (![before[@"generation"] isEqual:endpoint[@"generation"]] || ![before[@"pid"] isEqual:endpoint[@"pid"]] ||
      ![before[@"port"] isEqual:endpoint[@"port"]] ||
      ![@[@"ready",@"ok"] containsObject:before[@"status"]] ||
      [before[@"last_id"] longLongValue] != [request[@"request_id"] longLongValue]-1)
    return @{@"input":@"unresolved", @"error":@"text_cursor_changed", @"source_request":request};
  NSDictionary *reply = uikit_control_exchange_bounded(endpoint,request,256*1024,1024*1024);
  if (!reply) reply = uikit_text_receipt(descriptor);
  if (![reply[@"request"] isEqual:request] || ![reply[@"generation"] isEqual:endpoint[@"generation"]] ||
      ![reply[@"pid"] isEqual:endpoint[@"pid"]] || ![reply[@"port"] isEqual:endpoint[@"port"]] ||
      ![reply[@"last_id"] isEqual:request[@"request_id"]] || ![reply[@"status"] isEqual:@"ok"] ||
      ![reply[@"replayed"] isEqual:@NO])
    return @{@"input":@"unresolved", @"error":@"text_delivery_unconfirmed",
      @"source_request":request, @"source_reply":reply ?: (id)NSNull.null};
  return @{@"input":@"uikit_text_control", @"receipt":reply};
}
