import Foundation

/// Protocol defining the network contract.
public protocol NetworkServiceProtocol {
    func fetch<T: Decodable>(from url: URL) async throws -> T
    func post<T: Encodable>(to url: URL, body: T) async throws
}

/// Concrete implementation with generic request handling and extensions.
public class NetworkService: NetworkServiceProtocol {
    private let session: URLSession
    private let decoder: JSONDecoder

    public init(session: URLSession = .shared) {
        self.session = session
        self.decoder = JSONDecoder()
    }

    public func fetch<T: Decodable>(from url: URL) async throws -> T {
        let data = try await loadData(from: url)
        return try decode(data)
    }

    public func post<T: Encodable>(to url: URL, body: T) async throws {
        var request = buildRequest(url: url, method: "POST")
        request.httpBody = try encode(body)
        try await performRequest(request)
    }

    private func loadData(from url: URL) async throws -> Data {
        let (data, response) = try await session.data(from: url)
        try validateResponse(response)
        return data
    }

    private func decode<T: Decodable>(_ data: Data) throws -> T {
        return try decoder.decode(T.self, from: data)
    }

    private func encode<T: Encodable>(_ value: T) throws -> Data {
        return try JSONEncoder().encode(value)
    }

    private func buildRequest(url: URL, method: String) -> URLRequest {
        var request = URLRequest(url: url)
        request.httpMethod = method
        return request
    }

    private func validateResponse(_ response: URLResponse) throws {
        guard let http = response as? HTTPURLResponse, http.statusCode < 400 else {
            throw NetworkError.badStatus
        }
    }

    private func performRequest(_ request: URLRequest) async throws {
        let (_, response) = try await session.data(for: request)
        try validateResponse(response)
    }
}

extension NetworkService {
    /// Convenience subscript to retrieve cached response data by URL string key.
    subscript(key: String) -> Data? {
        return cache(for: key)
    }

    private func cache(for key: String) -> Data? {
        return nil
    }
}

enum NetworkError: Error {
    case badStatus
    case invalidResponse
}
