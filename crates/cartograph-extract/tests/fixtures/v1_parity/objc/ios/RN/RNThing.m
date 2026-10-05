#import <React/RCTBridgeModule.h>
#import <React/RCTEventEmitter.h>

@interface RNThing : RCTEventEmitter <RCTBridgeModule>
@end

@implementation RNThing

RCT_EXPORT_MODULE()

RCT_EXPORT_METHOD(doSomething:(NSString *)name resolver:(RCTPromiseResolveBlock)resolve)
{
  [self helper];
  resolve(@"ok");
}

- (void)plainMethod:(NSInteger)x with:(NSInteger)y
{
  [self helper];
}

- (void)helper
{
  [self sendEventWithName:@"thingChanged" body:@{}];
}

RCT_REMAP_METHOD(getThing, getThingWithResolver:(RCTPromiseResolveBlock)resolve)
{
  resolve(@"thing");
}

RCT_EXPORT_BLOCKING_SYNCHRONOUS_METHOD(syncName)
{
  return @"v";
}

RCT_REMAP_BLOCKING_SYNCHRONOUS_METHOD(getMap, NSDictionary<NSString *, id> *, getMapValue)
{
  return @{};
}

RCT_EXPORT_METHOD (spacedRun:(NSString *)arg)
{
  saveEvent(arg);
}

RCT_EXPORT_METHOD(addListener:(NSString *)eventName)
{
}

- (NSArray<NSString *> *)supportedEvents
{
  return @[@"thingChanged"];
}

@end

@implementation RCTGeolocation
RCT_EXPORT_METHOD(getCurrentPosition:(RCTResponseSenderBlock)cb)
{
  cb(@[]);
}
@end
