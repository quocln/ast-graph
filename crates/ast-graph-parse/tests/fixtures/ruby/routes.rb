# Typical Rails config/routes.rb
# Demonstrates: resources, nested routes, member/collection blocks,
# namespace, root route, and direct HTTP verb routes.
Rails.application.routes.draw do
  root to: 'home#index'

  resources :users do
    member do
      get :profile
      post :activate
    end
    collection do
      get :search
    end
    resources :posts, only: [:index, :show]
  end

  resources :posts do
    resources :comments, only: [:create, :destroy]
    member do
      post :publish
      delete :unpublish
    end
    collection do
      get :trending
    end
  end

  namespace :admin do
    resources :users
    resources :posts, except: [:new, :edit]
  end

  get  '/login',  to: 'sessions#new',     as: :login
  post '/login',  to: 'sessions#create'
  delete '/logout', to: 'sessions#destroy', as: :logout
end
