package main

import (
	"log"
	"net/http"

	"github.com/gin-gonic/gin"
	"github.com/go-chi/chi/v5"
	"github.com/spf13/cobra"

	"example.com/shop/internal/handlers"
)

var rootCmd = &cobra.Command{
	Use:   "shop",
	Short: "shop server",
}

var serveCmd, migrateCmd = &cobra.Command{Use: "serve"}, &cobra.Command{Use: "migrate up"}

func health(w http.ResponseWriter, r *http.Request) {}

func setupGin() *gin.Engine {
	g := gin.Default()
	g.GET("/ping", func(c *gin.Context) { c.String(200, "pong") })
	g.POST("/orders", createOrder)
	return g
}

func createOrder(c *gin.Context) {}

func setupChi() http.Handler {
	r := chi.NewRouter()
	r.Get("/items", listItems)
	r.Delete("/items/{id}", deleteItem)
	return r
}

func listItems(w http.ResponseWriter, r *http.Request)  {}
func deleteItem(w http.ResponseWriter, r *http.Request) {}

func main() {
	mux := http.NewServeMux()
	http.HandleFunc("/health", health)
	http.Handle("/static/", http.FileServer(http.Dir("./public")))
	mux.HandleFunc("GET /api/users/{id}", handlers.HandleList)
	rootCmd.AddCommand(serveCmd, migrateCmd)
	setupGin()
	setupChi()
	if err := rootCmd.Execute(); err != nil {
		log.Fatal(err)
	}
}
