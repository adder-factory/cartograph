import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';
import 'app.dart';
import 'router.dart';
import 'services/user_service.dart';

final router = GoRouter(routes: appRoutes);

void main() {
  runService();
  runApp(const App());
}
