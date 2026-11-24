"""Prepare a small platform-owned XCTest project; no consumer source or app hooks are required."""
import argparse
import hashlib
import json
import plistlib
from pathlib import Path
import shutil


def prepare(output):
    root = Path(__file__).resolve().parents[1]
    output.mkdir(parents=True, exist_ok=False)
    native = root/'spikes/s18-native-time/ios'
    project = output/'SystemUI.xcodeproj'
    shutil.copytree(native/'NativeProbe.xcodeproj', project)
    for path in project.rglob('*'):
        if path.is_file():
            value = path.read_text().replace('NativeProbe', 'SystemUI').replace(
                'dev.frontcap.s18.probe', 'dev.frontcap.system-ui').replace(
                'dev.frontcap.s18.uitests', 'dev.frontcap.system-ui.tests')
            if path.name == 'project.pbxproj':
                # The placeholder target contains no clock library or consumer code.
                value = '\n'.join(line for line in value.splitlines()
                                  if not line.lstrip().startswith(('A00000000000000000000008 =', 'A00000000000000000000016 =')))+'\n'
                value = value.replace(', A00000000000000000000008', '').replace(', A00000000000000000000016', '')
                value = value.replace('INFOPLIST_KEY_UILaunchScreen_Generation = YES;',
                                      'INFOPLIST_KEY_UILaunchScreen_Generation = YES; INFOPLIST_FILE = SystemUI-Info.plist;')
            path.write_text(value)
    scheme = project/'xcshareddata/xcschemes/NativeProbe.xcscheme'
    scheme.rename(scheme.with_name('SystemUI.xcscheme'))
    files = ['packages/frontcap_uikit/'+name for name in
             ('system_tests.m', 'system_bridge.h', 'system_alerts.h', 'host_state.h', 'capture.h', 'capture_fault.h')]
    files += ['spikes/s18-native-time/ios/'+name for name in ('ui_control.h', 'control_transport.h')]
    for name in files:
        destination = output/'source'/name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(root/name, destination)
    (output/'app_tests.m').write_text('#import "source/packages/frontcap_uikit/system_tests.m"\n')
    (output/'SystemUI-Info.plist').write_bytes(plistlib.dumps({
        'UIApplicationSceneManifest': {
            'UIApplicationSupportsMultipleScenes': False,
            'UISceneConfigurations': {'UIWindowSceneSessionRoleApplication': [{
                'UISceneConfigurationName': 'Default', 'UISceneDelegateClassName': 'SystemScene',
            }]},
        },
    }))
    (output/'app.m').write_text('''#import <UIKit/UIKit.h>
@interface SystemScene : UIResponder <UIWindowSceneDelegate>
@property(nonatomic, strong) UIWindow *window;
@end
@implementation SystemScene
- (void)scene:(UIScene *)scene willConnectToSession:(UISceneSession *)session options:(UISceneConnectionOptions *)options {
  if (![scene isKindOfClass:UIWindowScene.class]) return;
  self.window=[[UIWindow alloc] initWithWindowScene:(UIWindowScene *)scene];
  UIViewController *controller=[UIViewController new];
  controller.view.backgroundColor=UIColor.systemBackgroundColor;
  self.window.rootViewController=controller;
  [self.window makeKeyAndVisible];
}
@end
@interface SystemHost : UIResponder <UIApplicationDelegate>
@end
@implementation SystemHost
@end
int main(int argc, char **argv) {
  @autoreleasepool { return UIApplicationMain(argc, argv, nil, NSStringFromClass(SystemHost.class)); }
}
''')
    inputs = {str(p.relative_to(output)): hashlib.sha256(p.read_bytes()).hexdigest()
              for p in sorted(output.rglob('*')) if p.is_file()}
    (output/'inputs.json').write_text(json.dumps(inputs, indent=2)+'\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=Path)
    prepare(parser.parse_args().output)
