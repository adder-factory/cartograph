#pragma once
#include <string>

class Sink {
public:
    virtual void write(const std::string &line) = 0;
};

class Logger {
public:
    void log(const std::string &msg);
protected:
    Sink *sink_ = nullptr;
};

class FileLogger : public Logger, public Sink {
public:
    void write(const std::string &line) override;
    void rotate() { log("rotate"); }
};

struct Point3 : Logger {
    int x, y, z;
};
