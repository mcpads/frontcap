// Admission/order regression checks; no simulator, app or synthesized input.
#import <Foundation/Foundation.h>
#include <math.h>
#include <assert.h>
#define FRONTCAP_TOUCH_PLANNING_ONLY
#include "touch_paths.h"

static NSArray *path(double start, double end) {
    return @[@{@"x":@0.25, @"y":@0.5, @"ms":@(start)},
        @{@"x":@0.25, @"y":@0.5, @"ms":@(end)}];
}
int main(void) {
    @autoreleasepool {
        NSArray *shortPath = path(0,300), *longPath = path(0,500);
        assert(touch_paths_valid(@[shortPath,longPath]));
        assert(([touch_path_order(@[shortPath,longPath]) isEqual:@[@1,@0]]));
        assert(([touch_path_order(@[longPath,shortPath]) isEqual:@[@0,@1]]));
        assert(([touch_path_order(@[shortPath,shortPath]) isEqual:@[@0,@1]]));
        assert(([touch_path_order(@[path(50,300),longPath]) isEqual:@[@1,@0]]));
        assert(!touch_paths_valid(@[]));
        assert(!touch_paths_valid(@[path(100,100)]));
        assert(!touch_paths_valid(@[path(0,5001)]));
        assert(!touch_paths_valid(@[@[@{@"x":@YES, @"y":@0.5, @"ms":@0},shortPath[1]]]));
        assert(!touch_paths_valid(@[@[@{@"x":@1.1, @"y":@0.5, @"ms":@0},shortPath[1]]]));
        NSMutableArray *longPoints = [NSMutableArray array];
        for (int i=0; i<17; i++)
            [longPoints addObject:@{@"x":@0.25, @"y":@0.5, @"ms":@(i)}];
        assert(!touch_paths_valid(@[longPoints,longPoints]));
    }
    puts("Touch admission and ordering checks passed.");
    return 0;
}
