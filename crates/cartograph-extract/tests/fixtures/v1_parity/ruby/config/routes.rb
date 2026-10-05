Rails.application.routes.draw do
  root "home#index"
  resources :orders
  resource :profile
  get "/health"
  post "/login"
  namespace :api do
    get "/status"
    delete "/sessions"
  end
  get "/a"; post "/b"
  root to: "pages#landing"
end
