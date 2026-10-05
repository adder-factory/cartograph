package demo

import java.time.Instant
import groovy.transform.CompileStatic
import groovy.util.logging.Slf4j
import static java.util.Objects.requireNonNull

interface Greeting {
    String greet(String other)
}

enum Tone {
    FORMAL, CASUAL

    String prefix() {
        return this == FORMAL ? 'Dear' : 'Hey'
    }
}

abstract class BaseGreeter implements Greeting {
    protected Tone tone = Tone.valueOf('CASUAL')

    String sign() {
        return 'base'
    }

    String describe() {
        return 'greeter'
    }
}

@Slf4j
@CompileStatic
class Greeter extends BaseGreeter {
    String name
    private final Instant createdAt = Instant.now()
    static final int MAX = 3

    Greeter(String name) {
        this.name = requireNonNull(name)
    }

    @Override
    String greet(String other) {
        return helper(other)
    }

    private String helper(String other) {
        def text = tone.prefix() + ' ' + other
        this.audit(text)
        return Formatter.format(text)
    }

    protected void audit(String text) {
        log.info(text)
    }

    @Override
    String sign() {
        return name
    }
}

class Formatter {
    static String format(String text) {
        return text.trim()
    }
}

def topLevel(value) {
    return value.toString()
}
