package controllers

import javax.inject._
import play.api.mvc._
import services.UserService

@Singleton
class Users @Inject() (cc: ControllerComponents, service: UserService) extends AbstractController(cc) {
  def show(id: Long) = Action {
    val user = service.load(id)
    Ok(user.name)
  }

  def create() = Action {
    Created(service.build(1L).name)
  }
}

@Singleton
class HomeController @Inject() (cc: ControllerComponents) extends AbstractController(cc) {
  def index() = Action {
    Ok("home")
  }
}
