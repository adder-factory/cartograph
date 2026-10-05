import Foundation

@objc class Player: NSObject {
    @objc func play(song: String) {
        prepare()
    }

    func prepare() {}

    @objc func download(url: String) {}
}

@objcMembers class Library: NSObject {
    func playForArtist(withOptions options: String) {}
    @nonobjc func hidden() {}
}

func useObjc(worker: Worker) {
    worker.greet()
    worker.other(3)
}
