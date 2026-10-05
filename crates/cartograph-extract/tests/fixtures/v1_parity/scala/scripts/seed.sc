import models.User

def seed(count: Int): Seq[User] = (1 to count).map(i => User.create(i.toLong))

val seeded = seed(3)
println(seeded.size)
