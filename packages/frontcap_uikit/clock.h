#import <UIKit/UIKit.h>
// Test-build integration. Inactive unless FC_MANUAL; enabled launches require the native runtime dylibs.
// Metadata runs on the untracked control thread; callers must not inspect UIKit from that block.
NSDictionary *start_native_clock(NSDictionary *(^metadata)(void), NSArray<NSString *> *diagnosticFiles);
void begin_native_clock_tracking(void);

// The test runner reads this window's accessibility value without consuming clock request IDs.
// Ordinary launches retain UIWindow behavior. FC_UI_INSPECT separately enables test-only text
// inspection without clock injection; it does not grant a text mutation endpoint.
@interface FCClockWindow : UIWindow
@end
