# ActiveRecord model for a User.
# Demonstrates: has_many, belongs_to, attr_accessor, scope, validates,
# before_save, after_create callbacks, include mixin, and instance variables.
class User < ApplicationRecord
  include Searchable
  include Auditable

  belongs_to :organization
  has_many :posts
  has_many :comments, class_name: 'Comment'
  has_one :profile

  attr_accessor :temporary_password, :skip_normalization

  scope :active,    -> { where(active: true) }
  scope :admins,    -> { where(role: 'admin') }

  validates :email, :name, presence: true
  validates :email, uniqueness: true
  validate :email_domain_allowed?

  before_save   :normalize_email
  after_create  :send_welcome_email
  after_create  :create_default_profile

  enum role: { member: 0, moderator: 1, admin: 2 }

  delegate :name, :avatar_url, to: :profile, prefix: true

  def initialize(attrs = {})
    super
    @display_cache = nil
    @@instance_count ||= 0
    @@instance_count += 1
  end

  def full_name
    "#{first_name} #{last_name}"
  end

  def display_name
    @display_cache ||= full_name
  end

  def activate!
    update!(active: true)
    self.after_activate
  end

  private

  def after_activate
    AuditLog.record(self, :activated)
  end

  def normalize_email
    self.email = email.downcase.strip
  end

  def send_welcome_email
    Mailer.welcome(self).deliver_later
  end

  def create_default_profile
    Profile.create!(user: self)
  end

  def email_domain_allowed?
    errors.add(:email, 'domain not allowed') unless AllowedDomain.permitted?(email)
  end
end
