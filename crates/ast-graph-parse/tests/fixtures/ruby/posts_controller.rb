# Rails controller for Posts resource.
# Demonstrates: before_action, private methods, standard CRUD actions,
# helper_method, and instance variables inside actions.
class PostsController < ApplicationController
  before_action :authenticate_user!
  before_action :find_post, only: [:show, :edit, :update, :destroy]
  before_action :authorize_post!, only: [:edit, :update, :destroy]

  helper_method :current_user_can_edit?

  def index
    @posts = Post.active.page(params[:page]).per(20)
  end

  def show
    @comments = @post.comments.order(created_at: :desc)
  end

  def new
    @post = Post.new
  end

  def create
    @post = current_user.posts.build(post_params)
    if @post.save
      redirect_to @post, notice: 'Post created.'
    else
      render :new, status: :unprocessable_entity
    end
  end

  def edit
  end

  def update
    if @post.update(post_params)
      redirect_to @post, notice: 'Post updated.'
    else
      render :edit, status: :unprocessable_entity
    end
  end

  def destroy
    @post.destroy
    redirect_to posts_path, notice: 'Post removed.'
  end

  private

  def find_post
    @post = Post.find(params[:id])
  end

  def authorize_post!
    redirect_to root_path unless current_user_can_edit?
  end

  def current_user_can_edit?
    current_user == @post.author || current_user.admin?
  end

  def post_params
    params.require(:post).permit(:title, :body, :published)
  end
end
