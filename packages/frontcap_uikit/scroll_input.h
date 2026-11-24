#pragma once
#import <UIKit/UIKit.h>
#import <objc/runtime.h>
#include <math.h>

// Source geometry is independent evidence for movement/end, not a substitute for XCTest hit testing.
static NSArray *inspect_uikit_scrolls(UIWindow *window) {
  if (!NSThread.isMainThread) return nil;
  NSMutableArray *pending=[NSMutableArray arrayWithObject:window], *rows=[NSMutableArray new];
  NSUInteger visited=0;
  while (pending.count) {
    UIView *view=pending.lastObject; [pending removeLastObject];
    if (++visited>2048) return nil;
    if (view.hidden || view.alpha<=0.01 || (view!=window && view.window!=window)) continue;
    [pending addObjectsFromArray:view.subviews];
    if (![view isKindOfClass:UIScrollView.class]) continue;
    UIScrollView *scroll=(UIScrollView *)view;
    NSString *identifier=view.accessibilityIdentifier ?: @"", *label=view.accessibilityLabel ?: @"";
    if (!identifier.length && !label.length) continue;
    if (identifier.length>4096 || label.length>4096) return nil;
    NSString *role=[view isKindOfClass:UITableView.class] ? @"table" :
      [view isKindOfClass:UICollectionView.class] ? @"collection_view" :
      [view isKindOfClass:UITextView.class] ? @"text_view" : @"scroll_view";
    static char tokenKey;
    NSString *token=objc_getAssociatedObject(view,&tokenKey);
    if (!token) { token=NSUUID.UUID.UUIDString.lowercaseString;
      objc_setAssociatedObject(view,&tokenKey,token,OBJC_ASSOCIATION_COPY_NONATOMIC); }
    UIEdgeInsets inset=scroll.adjustedContentInset;
    double minimum[2]={-inset.left,-inset.top};
    double maximum[2]={MAX(minimum[0],scroll.contentSize.width-scroll.bounds.size.width+inset.right),
      MAX(minimum[1],scroll.contentSize.height-scroll.bounds.size.height+inset.bottom)};
    double values[8]={scroll.contentOffset.x,scroll.contentOffset.y,minimum[0],minimum[1],maximum[0],maximum[1],
      scroll.bounds.size.width,scroll.bounds.size.height};
    NSMutableArray *numbers=[NSMutableArray new];
    for (int i=0;i<8;i++) { if (!isfinite(values[i]) || fabs(values[i]*1000)>INT32_MAX) return nil;
      [numbers addObject:@(llround(values[i]*1000))]; }
    [rows addObject:@{@"token":token,@"role":role,@"identifier":identifier,@"label":label,
      @"offset_mpt":[numbers subarrayWithRange:NSMakeRange(0,2)],
      @"minimum_mpt":[numbers subarrayWithRange:NSMakeRange(2,2)],
      @"maximum_mpt":[numbers subarrayWithRange:NSMakeRange(4,2)],
      @"viewport_mpt":[numbers subarrayWithRange:NSMakeRange(6,2)],
      @"enabled":(scroll.scrollEnabled && scroll.userInteractionEnabled ? @YES : @NO),
      @"dragging":@(scroll.dragging),@"decelerating":@(scroll.decelerating)}];
  }
  return rows;
}
