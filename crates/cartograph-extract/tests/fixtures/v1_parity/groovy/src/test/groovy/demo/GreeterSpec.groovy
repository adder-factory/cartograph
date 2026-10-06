package demo

import spock.lang.Specification

class GreeterSpec extends Specification {
    def "greets with a tone"() {
        given:
        def greeter = new Greeter('qa')

        expect:
        greeter.greet('x').endsWith('x')
    }
}
