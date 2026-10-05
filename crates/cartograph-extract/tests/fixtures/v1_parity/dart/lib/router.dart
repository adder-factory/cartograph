import 'package:go_router/go_router.dart';
import 'screens/screens.dart';

final appRoutes = <RouteBase>[
  GoRoute(
    path: '/',
    builder: (context, state) => const HomeScreen(),
    routes: [
      GoRoute(path: 'details', builder: (context, state) => const DetailsScreen()),
    ],
  ),
  GoRoute(path: '/profile', builder: (ctx, s) => ProfileScreen()),
];
