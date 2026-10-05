import 'dart:convert';
import '../utils/helpers.dart' as helpers;

typedef Callback = void Function(User user);

enum Color { red, green, blue }

enum Role {
  admin('A'),
  member('M');

  final String code;
  const Role(this.code);
}

mixin Logger {
  void log(String msg) {
    print(msg);
  }
}

extension StrX on String {
  String shout() => toUpperCase();
}

abstract class Repo {
  Future<User?> findById(String id);
}

class Base {
  void init() {}
}

class User {
  final String name;
  int _age;
  static const int maxAge = 150;
  Role role = Role.member;

  User(this.name, this._age);

  User.guest() : name = 'guest', _age = 0;

  factory User.fromJson(Map<String, dynamic> json) {
    return User(json['name'] as String, json['age'] as int);
  }

  int get age => _age;

  set age(int value) {
    _age = _validate(value);
  }

  String display() {
    return helpers.formatName(name, role.code);
  }

  int _validate(int v) => v > maxAge ? maxAge : v;

  static User anonymous() => User.guest();

  String toJson() => jsonEncode({'name': name});
}

class UserRepo extends Base with Logger implements Repo {
  final Map<String, User> _cache = {};

  @override
  Future<User?> findById(String id) async {
    log('find $id');
    init();
    return _cache[id] ?? User.anonymous();
  }

  void _privateMethod() {}
}
