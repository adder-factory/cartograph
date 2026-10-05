import ExpoModulesCore

class Helper {
    func doX() {}
}

public class HapticsModule: Module {
    public func definition() -> ModuleDefinition {
        Name("Haptics")
        Constants("PI")
        Function("impact") { (style: String) in
            Helper().doX()
        }
        AsyncFunction("notify") { (kind: String) in
            return kind
        }
        Function ("spaced") { }
        Property("isSupported") { true }
        Function("addListener") { (name: String) in }
    }
}

public class BareModule: Module {
    public func definition() -> ModuleDefinition {
        Function("doBare") { return 1 }
    }
}
