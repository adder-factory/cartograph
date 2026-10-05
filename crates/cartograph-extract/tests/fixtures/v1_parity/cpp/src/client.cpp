#include "client.hpp"

Client Client::create() {
    return Client{};
}

void Client::commit() {
    retries++;
}

int deal_command(std::string interfaceName, int flag) {
    return flag;
}

API_EXPORT int api_foo(void) {
    return 1;
}
