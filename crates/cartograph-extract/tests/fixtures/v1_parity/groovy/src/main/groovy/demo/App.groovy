package demo

import demo.Greeter

class App {
    static void main(String[] args) {
        def greeter = new Greeter('world')
        println greeter.greet('team')
        runAll(greeter)
    }

    static void runAll(Greeter greeter) {
        greeter.sign()
        Formatter.format('done')
    }
}
