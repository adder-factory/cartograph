#pragma once

class Client {
public:
    static Client create();
    void commit();
    int retries = 0;
};

SOME_TEMPLATE_MACRO
class Pooled {
    void acquire() {}
    void release() {}
};

MY_MACRO
struct pod_t {
    int x;
};
