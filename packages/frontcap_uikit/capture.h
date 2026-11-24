#pragma once
// Bounded XCTest raster/hierarchy artifacts shared by app and system observations.
#import <XCTest/XCTest.h>
#import <CommonCrypto/CommonDigest.h>
#import "capture_fault.h"

static NSArray *uikit_capture_description(NSString *description, NSString *directory, NSString *observation) {
  NSString *relative = [@"observations" stringByAppendingPathComponent:observation];
  NSString *folder = [directory stringByAppendingPathComponent:relative];
  if (![NSFileManager.defaultManager createDirectoryAtPath:folder withIntermediateDirectories:YES attributes:nil error:NULL])
    @throw [NSException exceptionWithName:@"UICapture" reason:@"Cannot create capture directory" userInfo:nil];
  NSData *hierarchy = [description dataUsingEncoding:NSUTF8StringEncoding];
  NSData *screen = XCUIScreen.mainScreen.screenshot.PNGRepresentation;
  NSMutableArray *artifacts = [NSMutableArray new];
  for (NSString *name in @[@"screen.png", @"hierarchy.txt"]) {
    NSData *data = [name isEqual:@"screen.png"] ? screen : hierarchy;
    if (!data.length || data.length > 16*1024*1024 ||
        ![data writeToFile:[folder stringByAppendingPathComponent:name] options:NSDataWritingAtomic error:NULL])
      @throw [NSException exceptionWithName:@"UICapture" reason:@"Cannot publish bounded capture" userInfo:nil];
    if ([name isEqual:@"screen.png"]) interrupt_observation_after_screen(directory, observation);
    unsigned char digest[CC_SHA256_DIGEST_LENGTH]; CC_SHA256(data.bytes, (CC_LONG)data.length, digest);
    NSMutableString *hash = [NSMutableString new];
    for (NSUInteger i = 0; i < sizeof(digest); i++) [hash appendFormat:@"%02x", digest[i]];
    [artifacts addObject:@{@"path":[relative stringByAppendingPathComponent:name], @"bytes":@(data.length), @"sha256":hash}];
  }
  return artifacts;
}
