"""Build entry for an existing controlled Flutter session without the SDK test relay.

Generate outside the consumer source tree, then build it as the Flutter --target. The existing
session and test configuration remain unchanged. The resulting app can start through ordinary OS
launch after termination; running flutter test again would reinstall its product.
"""
import argparse
from pathlib import Path


def prepare(project, output):
    project = project.resolve(strict=True)
    configuration = project/'integration_test/flutter_test_config.dart'
    session = project/'integration_test/session_test.dart'
    if not configuration.is_file() or not session.is_file():
        raise ValueError('Missing existing controlled Flutter session/configuration')
    def uri(path):
        # Percent-encoded file URIs preserve source paths as Dart string literals.
        return path.as_uri().replace("'", '%27').replace('$', '%24')
    with output.open('x') as stream:
        stream.write("import '"+uri(configuration)+"' as configuration;\n"
                     "import '"+uri(session)+"' as session;\n"
                     "Future<void> main() async { await configuration.testExecutable(session.main); }\n")


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('project', type=Path)
    parser.add_argument('output', type=Path)
    args = parser.parse_args()
    prepare(args.project, args.output)
