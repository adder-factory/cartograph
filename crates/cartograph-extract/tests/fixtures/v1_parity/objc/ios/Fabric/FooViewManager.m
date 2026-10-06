#import <React/RCTViewManager.h>

@interface FooHelper : NSObject
@end
@implementation FooHelper
@end

@interface FooViewManager : RCTViewManager
@end

@implementation FooViewManager
RCT_EXPORT_MODULE(FooView)
RCT_EXPORT_VIEW_PROPERTY(color, NSString)
RCT_CUSTOM_VIEW_PROPERTY(radius, NSNumber, UIView)
{
}
RCT_REMAP_VIEW_PROPERTY(label, accessibilityLabel, NSString)

- (UIView *)view
{
  return [[UIView alloc] init];
}
@end
