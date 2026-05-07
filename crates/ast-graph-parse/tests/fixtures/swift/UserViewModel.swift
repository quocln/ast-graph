import Foundation
import Combine

/// View model for user profile data — demonstrates ObservableObject, @Published, async/await.
public class UserViewModel: ObservableObject {
    @Published public var name: String = ""
    @Published public var email: String = ""
    @Published private var isLoading: Bool = false

    private var cancellables: Set<AnyCancellable> = []
    private let repository: UserRepository

    public init(repository: UserRepository) {
        self.repository = repository
        setupBindings()
    }

    private func setupBindings() {
        repository.userPublisher
            .receive(on: DispatchQueue.main)
            .sink { [weak self] user in
                self?.applyUser(user)
            }
            .store(in: &cancellables)
    }

    private func applyUser(_ user: User) {
        name = user.name
        email = user.email
    }

    public func loadUser(id: String) async throws {
        isLoading = true
        defer { isLoading = false }
        let user = try await repository.fetchUser(id: id)
        applyUser(user)
    }

    public func saveUser() async throws {
        let snapshot = buildSnapshot()
        try await repository.save(snapshot)
    }

    private func buildSnapshot() -> User {
        return User(name: name, email: email)
    }
}
