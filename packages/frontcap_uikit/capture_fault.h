#pragma once
#import <Foundation/Foundation.h>
#include <stdlib.h>
#include <unistd.h>

// Opt-in fault control: exit only for the exact executing observation selected by its owner.
static void interrupt_observation_after_screen(NSString *directory, NSString *capture) {
  NSData *armed = [NSData dataWithContentsOfFile:[directory stringByAppendingPathComponent:@"observation-exit.json"]];
  if (!armed) return;
  NSDictionary *request = [NSJSONSerialization JSONObjectWithData:armed options:0 error:NULL];
  NSData *data = [NSData dataWithContentsOfFile:[directory stringByAppendingPathComponent:@"ui-control.json"]];
  NSDictionary *receipt = data ? [NSJSONSerialization JSONObjectWithData:data options:0 error:NULL] : nil;
  if (![@"executing" isEqual:receipt[@"status"]] || ![@[@"observe", @"observe_targets"] containsObject:request[@"operation"]] ||
      ![request isEqual:receipt[@"request"]]) return;
  NSDictionary *checkpoint = @{@"phase":@"after-screen-before-hierarchy", @"pid":@(getpid()),
    @"capture_id":capture, @"receipt":receipt};
  data = [NSJSONSerialization dataWithJSONObject:checkpoint options:NSJSONWritingPrettyPrinted error:NULL];
  if (![data writeToFile:[directory stringByAppendingPathComponent:@"observation-exit-checkpoint.json"]
                   options:NSDataWritingAtomic error:NULL]) abort();
  _Exit(78); // No terminal receipt; the partial capture must stay unresolved.
}
