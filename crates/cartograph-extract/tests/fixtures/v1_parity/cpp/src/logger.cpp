#include "logger.hpp"

void Logger::log(const std::string &msg) {
    if (sink_) {
        sink_->write(msg);
    }
}

void FileLogger::write(const std::string &line) {
    rotate();
}

FileLogger *make_logger() {
    FileLogger *f = new FileLogger();
    f->log("start");
    return f;
}
