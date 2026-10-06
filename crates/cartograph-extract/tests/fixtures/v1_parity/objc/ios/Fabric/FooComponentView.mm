#import "FooComponentView.h"

@interface FooComponentView : UIView
@end

@implementation FooComponentView
- (instancetype)initWithFrame:(CGRect)frame
{
  if (self = [super initWithFrame:frame]) {
    [self setup];
  }
  return self;
}

- (void)setup
{
}
@end
