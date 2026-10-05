#import <Foundation/Foundation.h>
#include "Constants.h"

typedef struct {
    int x;
    int y;
} WorkerPoint;

typedef enum {
    WorkerStateIdle,
    WorkerStateBusy
} WorkerState;

@protocol WorkerDelegate <NSObject>
- (void)workerDidFinish:(id)worker;
@optional
- (NSInteger)numberOfItems;
@end

@interface Worker : NSObject <NSCopying, WorkerDelegate>
@property (nonatomic, copy) NSString *name;
@property (nonatomic, strong) NSMutableArray *items;
@property (nonatomic, weak) id<WorkerDelegate> delegate;
@property (nonatomic, assign) WorkerState state;
- (void)greet;
- (void)doThing:(id)x with:(id)y;
- (void)other:(int)n;
+ (instancetype)shared;
@end

@interface MyMap<KeyType, ObjectType> : NSObject <NSCopying>
- (void)setObject:(ObjectType)obj forKey:(KeyType)key;
@end
