"""Generate a platform-owned XCTest harness; never edits the consumer project."""
import argparse
import plistlib
from pathlib import Path
import re
import shutil

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--output', required=True, type=Path)
parser.add_argument('--bundle-id', required=True)
args = parser.parse_args()
if not re.fullmatch(r'[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+', args.bundle_id):
    parser.error('Invalid consumer bundle identifier')
args.output.mkdir(parents=True, exist_ok=False)
root = Path(__file__).resolve().parent
native = root.parent / 's18-native-time/ios'
project = args.output / 'WebViewInput.xcodeproj'
shutil.copytree(native / 'NativeProbe.xcodeproj', project)
for path in list(project.rglob('*')):
    if path.is_file():
        path.write_text(path.read_text().replace('NativeProbe', 'WebViewInput')
                        .replace('dev.frontcap.s18.probe', 'dev.frontcap.webview.input')
                        .replace('dev.frontcap.s18.uitests', 'dev.frontcap.webview.input.tests')
                        .replace('INFOPLIST_KEY_UILaunchScreen_Generation = YES;',
                                 'INFOPLIST_KEY_UILaunchScreen_Generation = YES; INFOPLIST_FILE = WebViewInput-Info.plist;'))
scheme = project / 'xcshareddata/xcschemes/NativeProbe.xcscheme'
scheme.rename(scheme.with_name('WebViewInput.xcscheme'))
for name in ('xctest_wait.h', 'ui_control.h', 'control_transport.h', 'clock_control.m', 'clock_control.h'):
    shutil.copy2(native / name, args.output / name)
shutil.copy2(root / 'input_tests.m', args.output / 'app_tests.m')
shutil.copy2(root / 'touch_paths.h', args.output / 'touch_paths.h')
(args.output / 'consumer.h').write_text(f'#define FC_XCTEST_BUNDLE @"{args.bundle_id}"\n')
(args.output / 'WebViewInput-Info.plist').write_bytes(plistlib.dumps({
    'UIApplicationSceneManifest': {
        'UIApplicationSupportsMultipleScenes': False,
        'UISceneConfigurations': {'UIWindowSceneSessionRoleApplication': [{
            'UISceneConfigurationName': 'Default', 'UISceneDelegateClassName': 'InputScene',
        }]},
    },
}))
(args.output / 'app.m').write_text('''#import <UIKit/UIKit.h>
@interface InputScene : UIResponder <UIWindowSceneDelegate>
@property(nonatomic, strong) UIWindow *window;
@end
@implementation InputScene
- (void)scene:(UIScene *)scene willConnectToSession:(UISceneSession *)session options:(UISceneConnectionOptions *)options {
  if (![scene isKindOfClass:UIWindowScene.class]) return;
  self.window=[[UIWindow alloc] initWithWindowScene:(UIWindowScene *)scene];
  UIViewController *controller=[UIViewController new];
  controller.view.backgroundColor=UIColor.systemBackgroundColor;
  self.window.rootViewController=controller;
  [self.window makeKeyAndVisible];
}
@end
@interface InputHost : UIResponder <UIApplicationDelegate>
@end
@implementation InputHost
@end
int main(int argc, char **argv) {
  @autoreleasepool { return UIApplicationMain(argc, argv, nil, NSStringFromClass(InputHost.class)); }
}
''')
