#import "Worker.h"
#import <UIKit/UIKit.h>

static int helperCount = 0;

void helperFunction(int count) {
    Worker *obj = [Worker shared];
    [obj greet];
    helperCount += count;
}

@implementation Worker

+ (instancetype)shared {
    return [[Worker alloc] init];
}

- (void)greet {
    NSLog(@"Hello %@", self.name);
    [self doWork];
}

- (void)doWork {
    [self doThing:self.name with:nil];
    [self other:WORKER_MAX];
}

- (void)doThing:(id)x with:(id)y {
    [self.items addObject:x];
    [self.delegate workerDidFinish:self];
}

- (void)other:(int)n {
    helperFunction(n);
    NSMutableDictionary *store = [NSMutableDictionary new];
    [store setValue:@(n) forKey:@"n"];
}

- (id)copyWithZone:(NSZone *)zone {
    return [Worker shared];
}

- (void)workerDidFinish:(id)worker {
}

@end

@implementation MyMap
- (void)setObject:(id)obj forKey:(id)key {
}
@end
