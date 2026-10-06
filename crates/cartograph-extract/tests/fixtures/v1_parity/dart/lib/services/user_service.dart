import 'dart:async';
import 'package:http/http.dart' as http;
import '../models/user.dart';
import '../utils/helpers.dart';
export '../models/user.dart' show User, UserRepo;

class UserService {
  final UserRepo repo;
  final http.Client client;

  UserService(this.repo, this.client);

  Future<User?> load(String id) async {
    await delay(10);
    final user = await repo.findById(id);
    user?.display();
    return user;
  }

  Future<String> greet(String id) async {
    final user = await load(id);
    final name = formatName(user?.name ?? 'x', 'y');
    return name.shout();
  }

  User create(Map<String, dynamic> json) {
    final created = User.fromJson(json);
    final guest = new User('a', 1);
    created.toJson();
    return guest;
  }
}

void runService() {
  final service = UserService(UserRepo(), http.Client());
  service.greet('1');
  retries();
}
