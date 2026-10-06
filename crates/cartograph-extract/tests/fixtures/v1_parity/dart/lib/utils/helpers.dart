const String appName = 'oracle';
final int maxRetries = 3;

String formatName(String first, String last) {
  return '$first $last'.trim();
}

Future<void> delay(int ms) async {
  await Future.delayed(Duration(milliseconds: ms));
}

int _clamp(int v) => v < 0 ? 0 : v;

int retries() => _clamp(maxRetries);
