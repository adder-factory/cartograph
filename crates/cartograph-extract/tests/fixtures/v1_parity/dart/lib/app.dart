import 'package:flutter/material.dart';
import 'screens/screens.dart';

class App extends StatelessWidget {
  const App({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      routes: {
        '/': (_) => HomeScreen(title: 'a'),
        '/settings': (_) => SettingsScreen(),
      },
    );
  }
}
