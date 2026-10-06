public virtual with sharing class BaseService {
    protected Integer calls = 0;

    public virtual void log(String message) {
        calls++;
        System.debug(message);
    }
}
