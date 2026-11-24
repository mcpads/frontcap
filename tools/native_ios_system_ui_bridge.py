"""System-only mode of the durable XCTest bridge; never binds or controls a consumer app."""
from native_ios_ui_bridge import main

if __name__ == '__main__':
    main(system_only=True)
