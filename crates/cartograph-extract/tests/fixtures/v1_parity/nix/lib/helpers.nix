rec {
  double = n: n * 2;
  quadruple = n: double (double n);
  compose = f: g: x: f (g x);
}
