import neug
import neug as ng
from neug import Relationship, Node

db = neug.Database("shop")
catalog = neug.Graph("catalog")
person = ng.Vertex("User")
owns = Relationship("OWNS")
product = Node("Product")
again = neug.Graph("catalog")
