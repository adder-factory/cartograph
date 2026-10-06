#import "Worker.h"

@interface PlayerClient : NSObject
@end

@implementation PlayerClient
- (void)start:(Player *)player {
    [player playWithSong:@"x"];
    [player downloadWithUrl:@"y"];
}
@end
